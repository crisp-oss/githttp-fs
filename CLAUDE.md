# githttp-fs

Git-based Content Management System served over HTTP.

## What it is

githttp-fs is a single Rust binary that wraps git repositories and exposes them as a file-system-over-HTTP API. Each tenant gets its own git repository on disk. Clients can create, read, update, delete, and move `.md`/`.mdx` files via REST, and optionally pin the presentation order of any directory's entries. Every effective write produces a git commit (re-writing a file with unchanged content is a no-op). A configurable webhook fires after each commit so downstream systems (e.g. a read-only SQL database) can stay in sync.

Git is never exposed in the API surface — no git terminology appears in requests or responses.

## Project layout

```
src/
  main.rs          — server startup, router wiring, config loading
  config.rs        — TOML config types (ServerConfig, HooksConfig, HookEvent)
  state.rs         — AppState: Arc<Config>, reqwest::Client, per-tenant DashMap<Mutex>
  error.rs         — AppError enum with axum IntoResponse (JSON error bodies)
  git.rs           — all git2 operations (write, delete, move, list, commits, revert, rollback)
  hooks.rs         — async hook delivery with exponential backoff retry
  middleware.rs    — Bearer API key guard (axum middleware)
  seek.rs          — SeekOptions: line-based content windowing for file reads
  order.rs         — per-directory file order index: format, path rules, validation
  routes/
    mod.rs         — shared request types (AuthorRequest)
    files.rs       — GET/PUT/DELETE/POST on /:collection_id/:tenant_id/files and /:collection_id/:tenant_id/files/*path (POST dispatching on the /move and /reorder suffixes), plus POST /:collection_id/:tenant_id/batch/files/read and GET /:collection_id/:tenant_id/count/files
    order.rs       — GET/PUT/DELETE on /:collection_id/:tenant_id/order and /:collection_id/:tenant_id/order/*path
    replay.rs      — POST /:collection_id/:tenant_id/batch/replay/hook (webhook replay for downstream reconciliation)
    commits.rs     — commit list, commit detail, revert / point-in-time rollback
    tenant.rs      — DELETE /:collection_id/:tenant_id
```

## HTTP API

All routes are prefixed `/v1` and require `Authorization: Bearer <api_key>`.

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/v1` | Check that the API key is valid (`200` with body `{ "pong": true }`, or `401`) |
| `DELETE` | `/v1/:collection_id/:tenant_id` | Delete entire tenant repository |
| `GET` | `/v1/:collection_id/:tenant_id/files?prefix_path=&maximum_depth=&include_hidden_files=&file_name_starts_with=&include_date_from=&include_date_to=&include_date_type=&apply_order_index=&implicit_order_default_index=&page=&per_page=` | List tracked files as a tree; optional `apply_order_index` (default `false`) orders every level by that directory's stored file order index, and optional `implicit_order_default_index` (a number, unset by default) sets the index an entry the order index does not name is treated as holding — `0` or `-1` lifts every unordered entry *above* the ordered ones, unset leaves them behind them; optional `prefix_path` scopes the listing to a sub-directory (e.g. `?prefix_path=/docs`); optional `maximum_depth` limits how many directory levels deep the listing goes; optional `include_hidden_files` (default `false`) includes dot-prefixed entries; optional `file_name_starts_with` narrows the listing to files *and directories* whose leaf name begins with the given prefix, case-insensitively (a matched directory brings its contents along); it accepts either a bare string or a JSON-array string of prefixes (e.g. `?file_name_starts_with=["intro", "readme"]`), matching an entry whose name begins with *any* of them; optional `include_date_from`/`include_date_to` (RFC 3339 date-times) narrow the listing to files whose git date falls in the half-open window `[from, to)`, and `include_date_type` (`updated` default, or `created`) selects which date is compared; `page`/`per_page` paginate over the root-level entries of the listing (default 100, max 500) |
| `GET` | `/v1/:collection_id/:tenant_id/count/files?prefix_path=&maximum_depth=&include_hidden_files=&restrict_file_extensions=` | Count files and directories; `prefix_path`, `maximum_depth`, and `include_hidden_files` carry the same semantics as on the file listing route; optional `restrict_file_extensions` (a stringified JSON array, e.g. `["md", "mdx"]`) narrows the file count to files with one of those extensions |
| `GET` | `/v1/:collection_id/:tenant_id/files/*path?seek_from_line_starts_with=&seek_to_line_starts_with=&seek_lines_maximum=` | Read file content, plus its `position` in its parent directory's file order index (`-1` when unlisted); optional `seek_*` parameters narrow the response to a line window (see below) |
| `POST` | `/v1/:collection_id/:tenant_id/batch/files/read` | Batch-read several files in one request, with an optional shared seek window (overridable per file); capped by `limits.batch_read_maximum_files` |
| `HEAD` | `/v1/:collection_id/:tenant_id/files/*path?check_prefix_path=` | Check that a file exists (`200` or `404`, no body); optional `check_prefix_path` (default `false`) makes a folder at that path count as existing too |
| `PUT` | `/v1/:collection_id/:tenant_id/files/*path` | Create or update a file |
| `DELETE` | `/v1/:collection_id/:tenant_id/files/*path` | Delete a file; the optional body flag `allow_prefix_path_recurse` (default `false`) lets the path name a folder instead, deleting every file beneath it recursively in one commit |
| `POST` | `/v1/:collection_id/:tenant_id/files/*path/move` | Move / rename a file; the optional body flag `allow_prefix_path_recurse` (default `false`) lets the source name a folder instead, relocating its whole subtree in one commit |
| `POST` | `/v1/:collection_id/:tenant_id/files/*path/reorder` | Give the file the numerical `position` from the body inside its parent directory's file order index, shifting the entries at and after it down by one, or drop it from the index with `position: -1`; the optional body flag `allow_prefix_path` (default `false`) lets the path name a folder instead, positioning the folder itself among its siblings; commits and fires `order.updated` exactly as the order routes do |
| `GET` | `/v1/:collection_id/:tenant_id/order` and `/v1/:collection_id/:tenant_id/order/*path` | Read the file order stored for a directory (`/order` being the repository root); `404` when it has none |
| `PUT` | `/v1/:collection_id/:tenant_id/order` and `/v1/:collection_id/:tenant_id/order/*path` | Replace a directory's file order |
| `DELETE` | `/v1/:collection_id/:tenant_id/order` and `/v1/:collection_id/:tenant_id/order/*path` | Drop a directory's file order, reverting it to the default listing order |
| `POST` | `/v1/:collection_id/:tenant_id/batch/replay/hook` | Reconciliation: intersect the paths a downstream mirror holds with what this server holds, and replay one hook per file on the side `direction` selects — `delete` fires `file.deleted` for everything *outside* the intersection, `create` fires `file.created` for everything *inside* it. Optionally scoped and throttled. Commits nothing |
| `GET` | `/v1/:collection_id/:tenant_id/commits?page=&per_page=&file_path=&include_statistics=` | List commits, paginated (default 100, max 500); optional `file_path` filters to commits touching that file, following renames backward; optional `include_statistics` adds per-commit insertion/deletion/files-changed counts |
| `GET` | `/v1/:collection_id/:tenant_id/commits/:sha` | Commit detail with per-file diffs and snapshots |
| `POST` | `/v1/:collection_id/:tenant_id/commits/:sha/revert` | Revert a commit |
| `POST` | `/v1/:collection_id/:tenant_id/commits/:sha/rollback` | Roll the files that commit touched back to the state they had *at* it (point-in-time rollback) |

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
  "message": "optional commit message",
  "allow_prefix_path_recurse": false
}
```

**POST** `/files/*path/move` — move / rename a file
```json
{
  "author": { "name": "Valerian Saliou", "email": "valerian@example.com" },
  "destination": "new/path/to/file.md",
  "message": "optional commit message",
  "allow_prefix_path_recurse": false
}
```

`allow_prefix_path_recurse` is optional on both and defaults to `false`. See [Prefix-path (folder) operations](#prefix-path-folder-operations) below for what it permits.

**POST** `/files/*path/reorder` — position the file in its parent's file order index
```json
{
  "author": { "name": "Valerian Saliou", "email": "valerian@example.com" },
  "position": 2,
  "message": "optional commit message",
  "allow_prefix_path": false
}
```

`position` is required and must be a **number** — a zero-based index, so `0` puts the entry first; a non-number (a string, a fraction) is a `400`. It counts against the parent index's *other* entries: the entry is dropped from wherever it currently sits and re-inserted at `position`, shifting the entries at and after it down by one. Because the index is [sparse](#file-order-index), a position past its end is clamped to the tail rather than rejected — a caller cannot be expected to know how many siblings happen to be listed.

`position: -1` is the one accepted negative value, and it is the inverse operation: the entry is dropped from the index and not re-inserted, leaving it implicitly ordered again. Nothing else changes — the file or folder itself is untouched, exactly as when it is merely moved — and if it was the index's last entry the index is removed rather than stored empty, so the event is `order.deleted` instead of `order.updated`. `-1` is deliberately the same value the [read route](#response-shapes) reports as `position` for an unlisted file: what a caller reads back is what it can send. Any value below `-1` is a `400`.

`allow_prefix_path` is optional and defaults to `false`: only a file is positionable, and a folder path answers `404` like any other "not a file". Set it to `true` and the path may name a folder too, which positions **the folder itself** among its siblings — an index interleaves files and directories freely, so a folder takes a slot exactly as a file does, and it is stored in the canonical spelling (with a trailing slash, which the caller may also use on the path). Like `allow_prefix_path_recurse` on delete and move, the flag only *permits* — a file path behaves identically with it on. Unlike those two it carries no `_recurse` suffix because nothing recurses: a folder's position is one entry in one index, and the folder's contents are untouched (indexes inside it keep their own order, since they order a different directory).

The index is **read and shifted, never rebuilt from the directory**: a sibling that was never ordered stays unlisted, since being absent from the index is a state (it means "implicitly ordered") and not a gap to fill. A parent with no index yet gets one holding just this entry — unlike the implicit upkeep that never creates an index, this is an explicit request for a position, exactly as `PUT /order` is. The entry must exist in the last committed state (`404` otherwise, `-1` included — dropping the position of something that is gone is a caller bug, and implicit upkeep already handles the real deletion); the path is classified against HEAD's tree under the tenant write lock, so it cannot go stale before the commit it drives. `limits.allowed_extensions` does not apply (no path is being written). Asking for the state the index already holds is a no-op — no commit, no hook, HEAD's sha — in both directions: an entry already at `position`, and an already-unlisted entry sent `-1` (including when the directory has no index at all).

This route writes an order index, not a file, so it commits and delivers exactly as the `/order` routes do — one `order.updated` carrying the parent directory's complete resulting order, never a `file.*` event.

**POST** `/commits/:sha/revert` — revert a commit
```json
{
  "author": { "name": "Valerian Saliou", "email": "valerian@example.com" },
  "message": "optional commit message"
}
```

**POST** `/commits/:sha/rollback` — roll this commit's files back to this point in time
```json
{
  "author": { "name": "Valerian Saliou", "email": "valerian@example.com" },
  "message": "optional commit message"
}
```

Same body as the revert route — no paths are passed. Which files are in scope is read from `:sha` itself (the files that commit touched), and each of them is restored to the exact state it had **at** that commit, no matter how many commits changed them since. Files the commit never touched are left untouched. The `limits.allowed_extensions` whitelist is *not* applied, since the content comes from history under paths this server already committed.

**PUT** `/order` and `/order/*path` — replace a directory's file order
```json
{
  "author": { "name": "Valerian Saliou", "email": "valerian@example.com" },
  "order": ["intro.md", "getting-started/", "advanced.mdx"],
  "message": "optional commit message"
}
```

`order` is required and must hold at least one entry (an empty order is a `400` — that is what `DELETE` is for). Entries are **leaf names**, not paths: a nested path, `.`, `..`, `.git`, an empty name, a duplicate (after any trailing slash is stripped), or a reference to the index file itself are all `400`. A trailing slash marking a directory is accepted and normalised — the server stores directories with one and files without, whichever spelling the caller used.

Every entry must **exist in that directory** in the last committed state; an entry naming something absent is a `400`. The check runs against HEAD's tree under the tenant write lock, so it cannot go stale before the commit it drives. The order may still be *sparse*: entries must exist, but not every existing sibling need be listed. Writing the order the directory already holds is a no-op — no commit, no hook, HEAD's sha in the response — exactly as re-PUTting unchanged file content is. A directory that does not exist is a `404`; `limits.allowed_extensions` does not apply (the server, not the caller, decides this path).

**DELETE** `/order` and `/order/*path` — drop a directory's file order
```json
{
  "author": { "name": "Valerian Saliou", "email": "valerian@example.com" },
  "message": "optional commit message"
}
```

A directory with no stored order is a `404`: there is nothing to delete, and answering `200` would hide a caller mistake.

**POST** `/batch/replay/hook` — replay file hooks to reconcile a downstream mirror (no `author`: nothing is committed)
```json
{
  "direction": "delete",
  "files": ["docs/intro.md", "docs/removed.md"],
  "prefix_path": "/docs",
  "include_hidden_files": false,
  "delay_ms": 100
}
```

One route, one set operation, two directions. `files` is the list of paths the **downstream mirror** currently holds — never a list of things to act on here. The server intersects it with what it actually holds, and `direction` picks which side of that intersection is replayed:

| `direction` | Replays | Which files | Repairs |
|-------------|---------|-------------|---------|
| `delete` | `file.deleted` | Everything **outside** the intersection — the caller holds them, git does not | Orphaned rows the mirror kept after a missed deletion |
| `create` | `file.created` | Everything **inside** it — git holds them, so the mirror should too | Rows the mirror is missing, or whose content went stale |

`direction` is required and must be exactly `"delete"` or `"create"`; anything else is a `400`. Nothing on the git side is written in any way: no commit is created, no file is touched, and background maintenance is not armed. The response reports how many files the batch affected (see [Response shapes](#response-shapes)).

`files` is **optional**, and omitting it defaults it to every file git holds in scope. The two directions then fall out very differently, and that asymmetry is inherent to the set operation rather than a special case: `create` covers the whole scope (the common "push everything you have at me" reconciliation), while `delete` produces nothing at all, since git cannot be missing what it just listed. Sending `"files": []` explicitly is a `400` — omit the field instead. Each path is sanitised with the same rules as the read route's `*path` and must be unique after sanitisation (duplicates are a `400`). A path naming an [order index](#file-order-index) is a `400` rather than being silently dropped: it cannot legitimately be in a mirror's list (the index is invisible to every `/files` route), and because hook events are classified by path, letting one through would reach the receiver as an `order.deleted` and wipe a directory's stored order on the strength of a caller mistake.

`prefix_path` is optional and scopes the git-side snapshot to one folder, with the listing route's semantics (a non-existent folder scopes to nothing). Paths in `files` stay **repo-root-relative** — as they are everywhere else on this API and in every hook payload — so `prefix_path` acts as a guard rail rather than a join: an entry that does not sit under it is a `400`. Rejecting is what keeps both sides of the set operation on the same footing, since an out-of-scope entry would fall outside the intersection for a reason that has nothing to do with whether git holds it — and in the `delete` direction that reads as an orphan and drops a live row.

`include_hidden_files` (default `false`) is only meaningful **when `files` is omitted**, where it shapes the default set exactly as on the listing route. When `files` *is* given, the git-side snapshot always includes hidden files no matter what the flag says, because the set operation needs git's set to be maximal: a file hidden from the snapshot would fall outside the intersection and replay a `file.deleted` for a file that is very much still there. Order indexes are excluded from the snapshot either way, exactly as they are from every `/files` route — `order.*` events are never replayed, and neither are `file.updated` or `file.moved`.

`delay_ms` is optional. It pauses that many milliseconds *between* consecutive deliveries (never after the last one), capped at `60000`; a larger value is a `400`. It is a **throttle, not an ordering device** — hook delivery is already strictly sequential per repository — and it exists to spare a receiver from a sustained burst. Its cost is that a replay holds that repository's hook queue for `delay_ms × file_count`, so every commit accepted after the replay waits behind it. A caller wanting to go slower than the cap should replay in several `prefix_path`-scoped passes rather than raising the delay.

The route answers `400` when no `[hooks]` receiver is configured at all: the job would deliver nothing, and answering `200` with a file count for a reconciliation that did nothing is worse than an error.

One assumption the `create` direction makes about the receiver: since its replay set includes files the mirror already holds, the receiver must treat `file.created` as **insert-or-replace** rather than a bare `INSERT`. `created` rather than `updated` is deliberate — the case a replay is usually run for is a row the receiver never got, and an `UPDATE` handler would silently do nothing for exactly those.

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

Hidden entries — files *and* directories whose name starts with a dot, per the Unix convention (e.g. `.gitignore`, `.templates/`) — are excluded from the listing by default; a hidden directory is pruned wholesale, its subtree never walked. Pass `include_hidden_files=true` to include them. The filter applies before pagination, so page counts only cover visible entries. It applies to entry *names* only, not to `prefix_path` resolution: explicitly listing `?prefix_path=/.templates` returns that folder's (non-hidden) contents, mirroring `ls .templates/`.

The optional `file_name_starts_with` query parameter narrows the listing to entries whose *leaf name* (not full path) begins with the given prefix, compared case-insensitively (Unicode lower-casing, so `?file_name_starts_with=Intro` matches `intro.md`). It accepts two spellings: a bare string (a single prefix), or — mirroring `seek_from_line_starts_with`, since query parameters are strings — a JSON-array string of prefixes (e.g. `?file_name_starts_with=["intro", "readme"]`, URL-encoded), in which case an entry matches if its leaf name begins with *any* of the prefixes. A value whose first non-whitespace character is `[` is parsed as the array spelling and must be a valid JSON array of strings (else `400`); anything else is taken verbatim as a single prefix. An empty value, an empty array, or an empty prefix all return `400`. Both files and directories are matched: a matched file is returned as a leaf (its ancestor directories present purely as structure), and a matched directory is returned with its whole subtree expanded — every descendant file, whether or not its own name matches — so the caller sees inside the folder they found. A directory that neither matches nor contains a match is pruned, so a search result never contains a dead-end empty directory (a matched directory whose visible content is entirely filtered out still shows, as a childless node — it is itself the match). It composes with the other parameters: `prefix_path` scopes where the search runs, `maximum_depth` bounds how deep it descends uniformly (a match below the limit is never found, and a directory sitting *at* the limit renders as a childless stub even when it matched), and hidden entries stay excluded unless `include_hidden_files=true`. Because matches can be nested anywhere, this is the one listing mode that walks the whole in-scope tree before paginating (the off-page-directories-never-walked optimisation does not apply); pagination is still parent-based, windowing over the root-level entries of the *matched* tree. Like the plain listing, matching is on names alone — no blob is ever opened.

The optional `include_date_from` / `include_date_to` query parameters narrow the listing to files whose git date falls inside the half-open window `[from, to)` — `include_date_from` inclusive, `include_date_to` exclusive — each an RFC 3339 date-time (e.g. `2026-06-16T10:00:00Z`), strictly validated (any other spelling returns `400`). Each bound is independently optional (an open-ended range); when both are given, `from` must be strictly before `to` (equal bounds select nothing, so it is a `400`). The optional `include_date_type` selects which date is compared: `updated` (the default) is the most recent commit that touched the file, `created` is the oldest commit that introduced it under its current path (renames are *not* followed). `include_date_type` is always validated against those two values, but the date filter is only active — and its cost only paid — when at least one bound is present; passing `include_date_type` alone changes nothing and keeps the cheap tree-only fast path. This is the crucial caveat: unlike every other listing mode, a date filter cannot be answered from tree objects (a tree entry carries no timestamp), so it walks commit history. The walk is still blob-free (it compares tree/oid deltas per commit, with no patch, stats, or rename detection) but its cost scales with history length, not page size — `updated` stops as soon as every in-scope file has been dated, whereas `created` must reach the root of history. It composes with the other parameters: `prefix_path`/`maximum_depth`/`include_hidden_files` scope which files are candidates (a file below the depth limit is never a candidate, and a directory whose contents were not walked is dropped rather than shown as a date-unclassifiable stub), and `file_name_starts_with` intersects with it (a file must match both the name prefix and the date window). Directories are kept only as the structure leading to a surviving file, so an emptied directory is pruned. The response shape is unchanged — the filter only removes entries; per-file dates are not reported.

The optional `apply_order_index` query parameter (default `false`) orders every level of the listing by the file order index stored for the directory that level belongs to (see [File order index](#file-order-index) below). Listed entries come first, in index order, files and directories interleaved freely; everything the index does not name follows in the ordinary order (directories first, then alphabetical), and an index entry naming something that is not present simply ranks nothing.

The optional `implicit_order_default_index` query parameter (a number, unset by default) changes where those unnamed entries land: it is the index they are all treated as holding, so they no longer have to follow the ordered ones. On an equal index an unlisted entry sorts *before* a listed one, which is what makes `0` mean "on top" rather than "tied with the first" — so `implicit_order_default_index=0` (or any negative value, e.g. `-1`) lifts every unordered entry above the whole index, and `2` slots them between the index's second and third entries. Unlisted entries keep their ordinary relative order among themselves either way, and a directory with *no* index is untouched regardless (with nothing listed, a shared fallback index cannot reorder anything). Leaving the parameter unset keeps the original behaviour — unlisted entries last — and it is only read when `apply_order_index=true`; passing it alone changes nothing, exactly as `include_date_type` does without a date bound. The listing root's own order is applied *before* the page window is sliced — pagination is over root-level entries, so ordering them afterwards would page over the wrong sequence — and only the subtrees that made the page are descended, so the off-page optimisation survives. It composes with every other parameter: a search or date-filtered result is ordered in full before being paginated, and a depth-limited stub costs no index read. This is the one listing mode that opens blobs: one small index per directory actually rendered. It defaults to `false` so no existing caller's results change.

**GET** `/order` and `/order/*path` — a directory's stored file order
```json
{
  "directory": "docs/guides",
  "order": ["intro.md", "getting-started/", "advanced.mdx"]
}
```

`directory` is the sanitised path (`""` for the repository root). Entries come back in the canonical spelling the server stores: directories with a trailing slash, files without. A directory with no stored order is a `404` — not an empty `order` array — so "unordered" and "ordered as nothing" cannot be confused.

**GET** `/count/files` — file and directory count statistics
```json
{
  "files": 12,
  "directories": 3
}
```

The count walks the same tree as the listing route and shares its scoping parameters exactly: `prefix_path` roots the count at a sub-directory (a non-existent folder yields zero counts, same rejection rules for `..`, `.`, `.git`), `maximum_depth` bounds how many directory levels are descended (directories sitting at the limit are counted — they exist at a visible level, matching the listing's childless stubs — but their contents are not; `0` returns `400`), and hidden entries are excluded unless `include_hidden_files=true` (a hidden directory's whole subtree is pruned from both counts). There is no pagination — the response is two integers regardless of repository size.

The optional `restrict_file_extensions` query parameter is a JSON array of extensions as a string (query parameters are strings — same wire spelling as the `seek_*` prefix lists), e.g. `?restrict_file_extensions=["md", "mdx"]` URL-encoded. When set, only files carrying one of the given extensions are counted, compared case-insensitively; entries are normalised by trimming leading dots (`".md"` and `"md"` are equivalent), and extension-less files never match. Directories are counted regardless — they have no extension to compare. A non-array value, an empty array, or an empty entry returns `400`. Like the listing, counting is served from tree objects alone — no blob is ever opened.

**GET** `/files/*path` — read file
```json
{
  "path": "docs/intro.md",
  "content": "# Hello world\n...",
  "position": 2
}
```

`position` is the file's zero-based position in its parent directory's [file order index](#file-order-index), so a client rendering one file knows where it belongs among its siblings without a second request. It is `-1` when the index does not name the file — which is also the answer when the directory has no index at all, since from the caller's point of view those are the same state (nothing pins this file). A number rather than a `null` for the unordered case: positions are zero-based, so `-1` cannot collide with a real one, and the field's type stays stable for a client comparing or sorting on it. It is reported unconditionally — the cost is one small blob read, and only when the parent directory actually has an index. It reflects the last committed state, like everything else on this route, and it is *not* affected by `implicit_order_default_index` (that parameter shapes a listing's rendering; this is the stored fact).

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

The `files` array is index-aligned with the request's `files` array. Each slot is either a `{ path, content }` object (with the seek window applied, `path` in sanitised form), or `null` when that path does not exist in HEAD (or is a folder). It carries no `position`, unlike the single read route: a batch spans arbitrary directories, so ordering information would mean one index read per distinct parent for a caller that asked for content — when order matters, `GET /files?apply_order_index=true` answers it for a whole tree in one pass. `null` strictly means "not found": a file that exists but cannot be represented in JSON (invalid UTF-8) fails the whole request with a `422` naming the path. The tenant not existing at all is a `404`, as on the single read route.

**HEAD** `/files/*path` — check file existence. Responds `200` with an empty body when the file exists in the last committed state, `404` when it doesn't (including when the path points to a folder or the tenant doesn't exist). Blob content is never loaded, so this is cheaper than a GET.

The optional `check_prefix_path` query parameter (default `false`) widens the question from "is there a file here" to "is there anything here": with `check_prefix_path=true` a path resolving to a *folder* also answers `200`. This is the check a caller makes before recursing a delete or a move. Both kinds are read from the same HEAD tree entry — a folder is a tree entry exactly as a file is a blob entry — so the answer still costs one tree lookup and opens no blob; the filesystem is never consulted. With the parameter on, a trailing slash on the path is tolerated (`/docs/guides/` and `/docs/guides` are the same folder). The route cannot distinguish *which* kind matched: it is a bare `200`/`404`, so a caller that needs to know should read the parent's file listing.

### Prefix-path (folder) operations

The delete and move routes act on a single file by default. Setting `"allow_prefix_path_recurse": true` **in the request body** lets the same route act on a whole folder instead, and it is opt-in precisely because the operation is heavy and destructive — one request can rewrite or remove an unbounded number of files.

It is a body field, not a query parameter, because it changes *what the write does* — it belongs with `author`, `message`, and `destination`. Query parameters on this API shape reads (scoping, filtering, windowing); no write takes one. The existence check is the exception that proves the rule: `check_prefix_path` is a query parameter because `HEAD` carries no body at all.

The parameter does not *force* folder semantics; it only permits them. The route classifies the path against HEAD's tree under the tenant write lock and dispatches accordingly: a path resolving to a file runs the ordinary single-file operation, unchanged, and a path resolving to nothing answers `404` as usual. Only a path resolving to a folder enters the recursive operation. With the parameter absent or `false`, a folder path is simply "not a file" and answers `404` — so recursion can never be entered by accident. A trailing slash on the path (and, on the move route, on `destination`) is tolerated when the parameter is on.

Both recursive operations produce **exactly one commit** and **one hook per file**, delivered in order, so a downstream receiver applies them file by file and converges to the right state:

- **Recursive delete** — every file beneath the folder is removed. The commit tree is HEAD's tree minus the single directory entry (git's tree updater drops the subtree with it and prunes any parent directory left empty), so commit cost is proportional to path depth, not to the number of files removed; only the hook list scales with the file count. Auto-generated message: `"delete: docs/guides/"` — the trailing slash distinguishes a folder-wide deletion from a single-file one. One `file.deleted` hook fires per file.
- **Recursive move** — the folder's whole subtree is relocated under `destination`. Every file keeps its own leaf name (only the ancestor prefix changes), so one `file.moved` hook fires per file and downstream entity identity survives the move. Blob oids are reused verbatim (no content rehash); content is read once per file purely to fill that file's hook payload, exactly as the single-file move does. Auto-generated message: `"move: docs/guides/ -> docs/handbook/"`. The `destination` must not exist in any form (file or folder — the caller must delete it first) and must not sit *inside* the source, which would ask the folder to be moved into itself; both are a `400`.

The `limits.allowed_extensions` whitelist is not applied to a folder `destination` — it carries no extension of its own, and every file inside keeps its leaf name, so extensions are preserved by construction. It still applies normally when the source turns out to be a file, even with the parameter on (the check is simply deferred until the source kind is known).

A folder holding nothing this API can represent (no blobs at all — only submodule entries) is a no-op on both routes: no commit, no hook, and `commit_sha` is current HEAD, same contract as an unchanged PUT.

The repository root is not addressable: an empty path (`/` after sanitisation) is a `400` on both routes. Deleting everything remains the tenant route's job.

The reorder route takes a third flag of this family, `allow_prefix_path`, spelled without `_recurse` on purpose: it lets the path name a folder, but a folder's *position* is one entry in one index — nothing recurses, nothing is destroyed, and the folder's contents are untouched. Everything else is identical: a body field, opt-in, classified against HEAD under the write lock, permitting folder semantics rather than forcing them, and tolerating a trailing slash when on.

### File order index

Git has no ordering of its own — tree entries are name-sorted by definition and carry no metadata slot — so a presentation order has to be stored as data. githttp-fs stores it **per directory**: one index holding the leaf names of that directory's entries, in the order they should be presented. Ordering is a sibling-level concern, and scoping the storage the same way keeps two costs bounded: a reorder touches one small file (so its commit and its hook payload are proportional to one directory, not to the repository), and a folder move needs no index rewriting at all (entries are leaf names, so every index inside a relocated subtree is still correct once it travels with it).

The whole feature is optional. A repository with no index anywhere behaves exactly as before, and a caller that never passes `apply_order_index=true` and never subscribes to the `order.*` hook events cannot tell the feature exists.

**The index is a separate resource, never a file.** It is stored as a `.order.json` blob in the directory it orders, but that is an implementation detail on the same footing as git itself: the index is **invisible to every `/files` route** — listing, count, read, `HEAD`, and batch (where it comes back as `null`) — regardless of `include_hidden_files`, and the write routes refuse the path outright (`PUT`, and a move destination, answer `400` pointing at `/order`; a move source or a `DELETE` answers `404`, since to those it is simply not a file). That invisibility is what makes the format impossible to bypass: were the index an ordinary file, a client could `PUT` it directly and store anything, and a receiver would see a `file.updated` on a magic path it had to sniff, parse and diff instead of a real event.

**Two ways in.** `PUT /order[/*path]` replaces a whole directory's order at once — the bulk spelling, for a caller that knows the full sequence it wants. `POST /files/*path/reorder` moves *one* file to a numerical position inside its parent's index, shifting the rest down — the incremental spelling, for a caller that only knows where one thing should go (a drag-and-drop, say) and does not want to read, splice and re-send the whole list. Both go through the same validation, produce the same kind of commit, and deliver the same `order.updated` snapshot; the reorder route lives under `/files` because it is addressed by the *entry* being positioned, not by the directory holding the index. It positions files only unless the caller sets `allow_prefix_path: true`, which lets it position a folder among its siblings as well (an index interleaves the two freely).

**Three ways out.** A listing renders an index (`apply_order_index=true`), a `GET /order[/*path]` returns one directory's order verbatim, and a single file read reports its own `position` in its parent's index (`-1` when unlisted) so a client showing one file needs no second request to place it among its siblings.

**Sparse and stale-tolerant.** An index need not list every sibling — unlisted entries follow in the ordinary order (or wherever `implicit_order_default_index` puts them, when a listing asks for that). And an entry naming something that is no longer there is silently ignored on read. Writes are validated strictly against HEAD, so staleness should not arise from normal use, but a revert or a rollback can restore an index older than the files it names, and no listing may fail because of it.

**Upkeep rides in the same commit** as the file operation that triggers it, so a downstream order table never references a file that is gone:

| Operation | Effect on the index |
|-----------|---------------------|
| Delete a file or folder | Dropped from its parent's index, if listed |
| Rename inside one directory | Replaced **in place**, keeping its position |
| Move across directories | Dropped from the source index; appended to the destination index *only if one already exists* |
| Recursive folder delete | Parent's entry dropped; each index inside the subtree disappears with it |
| Recursive folder move | Parent entries updated; each index inside the subtree travels untouched (leaf names are unchanged) |
| Create a file (`PUT`) | Nothing — a new file is unlisted, so it sorts to the tail |
| Reorder a file or folder (`POST .../reorder`) | Re-inserted at the requested position, shifting the entries at and after it down — or, with `position: -1`, dropped from the index while the file itself stays; unlike the rows above it *creates* the index when the parent has none, since the caller asked for a position explicitly (a folder needs `allow_prefix_path: true`) |

Two rules in that table are deliberate rather than incidental. A rename keeps its position because demoting a file to the tail for changing its name would silently reorder content the caller only renamed. And *implicit* upkeep only ever edits an index, never creates one: appending to a directory that had no index would pin one file while all its siblings stayed implicitly ordered — a surprise the caller did not ask for. The reorder route is the deliberate exception, and it is not really one: a caller naming a position for a file has asked for exactly that pinning, so it creates the index the same way `PUT /order` does. An index left with no entries is removed rather than stored empty, since an empty index and no index are the same state.

**PUT / DELETE / POST move / POST reorder / PUT order / DELETE order** — write result
```json
{ "commit_sha": "a3f9c1d" }
```

A PUT whose `content` is byte-for-byte identical to what the file already holds is a no-op: no commit is created, no hook fires, and the response carries the current HEAD sha (the commit whose tree already contains that exact content).

A recursive delete or move returns the same single `commit_sha` — the whole folder travels in one commit regardless of how many files it holds.

**POST** `/batch/replay/hook` — replay result
```json
{
  "commit_sha": "a3f9c1d",
  "files": 12
}
```

`files` is the number of files the batch affected: how many `file.deleted` hooks will be replayed in the `delete` direction, or how many `file.created` ones in the `create` direction. `0` means the two sides already agreed on that side of the intersection, and nothing was enqueued.

The affected paths themselves are not echoed: in the `create` direction the set is bounded by the repository rather than by the request, so a whole-tenant replay would answer with the entire file list for no benefit. A caller that wants to see it can ask `GET /files`.

`commit_sha` is the HEAD the snapshot was taken from — no commit was created; it is the honest answer to "which state was this computed against".

The response returns as soon as the job is enqueued — delivery happens on the repository's hook queue afterwards, so a `200` means "scheduled", not "delivered". Progress and completion are visible in the logs (`replay starting` / `replay finished`, with delivered and skipped counts).

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

The optional `include_statistics=true` query parameter adds a `statistics` object to each commit:
```json
{ "insertions": 12, "deletions": 4, "files_changed": 2 }
```
This requires an actual content diff against each commit's parent (renames are similarity-detected first, so a pure rename doesn't count as a full delete+add), unlike the rest of this listing which is served from cheap tree/oid comparisons alone — so it's opt-in and its cost scales with `per_page`, not with total history size. Omitting the parameter (or `false`) leaves `statistics` out of the response entirely.

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
  ],
  "statistics": { "insertions": 12, "deletions": 4, "files_changed": 2 }
}
```

`change` is one of `"created"`, `"updated"`, `"deleted"`, `"moved"`. Moved files include an additional `"from_path"` field. `content` is empty string for deleted files. Unlike the commit list route, `statistics` is always present here — it is unconditional, computed from the same parent diff already built to derive `files`, so there is no extra diff pass to opt out of.

**POST** `/commits/:sha/revert`
```json
{
  "reverted_sha": "a3f9c1d",
  "commit_sha": "b8d2e4a"
}
```

**POST** `/commits/:sha/rollback`
```json
{
  "rolled_back_to_sha": "a3f9c1d",
  "commit_sha": "b8d2e4a"
}
```

For every path in `:sha`'s own change set, the rollback compares that path's state at `:sha` with its state in HEAD and commits the difference, so deletions travel in both directions:

| at `:sha` | at HEAD | result |
|-----------|---------|--------|
| exists | exists, different content | file updated → `file.updated` hook |
| exists | absent | file re-created → `file.created` hook (a since-deleted file comes back) |
| absent (the commit deleted it) | exists | file deleted again → `file.deleted` hook |
| exists | exists, identical content | that path is skipped entirely — no staging, no hook |

A rename inside `:sha` rolls back as a rename — one `file.moved` hook, preserving downstream entity identity — whenever HEAD still holds the pre-rename path and nothing sits at the post-rename one; otherwise each side is settled on its own (a restore plus a delete). A folder at a path counts as "absent" on either side, same as everywhere else in the API.

When no path needs to move (the repository already holds that state), the whole request is a no-op: no commit, no hook, and `commit_sha` is current HEAD. Rolling back *to* the initial commit is legal, unlike reverting it — with no parent, its change set is simply its whole tree.

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
# files written before the whitelist was configured remain movable. A folder
# move destination is not checked either — it carries no extension, and the
# files inside keep their own leaf names.
allowed_extensions = ["md", "mdx"]
# Safety cap on how many files one batch read request may ask for; larger
# requests are rejected with 400. Defaults to 100 if unset.
batch_read_maximum_files = 100

[hooks]
url = "https://your-receiver.example.com/hook"
# The four file events plus, optionally, the two file-order-index events.
# A receiver that does not list the order.* events gets none of them.
events = [
  "file.created", "file.updated", "file.deleted", "file.moved",
  "order.updated", "order.deleted"
]
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
- **Hook events are ordered per repository, and concurrent across repositories** — each `"{collection_id}/{tenant_id}"` gets a dedicated in-memory queue with exactly one consumer task, which awaits each job to full completion (every file, every retry, every backoff sleep) before taking the next. Jobs are enqueued while the tenant write lock is still held, so hooks are delivered in exactly the order commits were accepted by this server; a later commit can never overtake an earlier one at the receiver (even one stuck in retries). Files within a single commit are delivered one hook at a time in order. Different repositories are different tokio tasks, so a slow or down receiver for one never delays another — which is also why the payload carries `collection_id` alongside `tenant_id`: those two together are the ordering domain, and a receiver that keyed on `tenant_id` alone could interleave two collections' events for what looked like one tenant. The trade is unbounded latency, never lost ordering: a recursive delete of 10 000 files occupies its repository's queue for 10 000 sequential POSTs.
- **One hook per file, never batched** — a commit's change set becomes one queue job, and the consumer sends one HTTP POST per change in it. Multi-file operations (revert, rollback, recursive folder delete/move) therefore fan out to one event per file rather than one summary event, so a receiver applies them file by file with no special-casing; a recursive move emits one `file.moved` per file, each with its own `from`/`to`, instead of a delete/create wave.
- **Unchanged writes are no-ops** — a PUT with content identical to HEAD's blob creates no commit and fires no hook; the response returns HEAD's sha. Detection hashes the incoming content and compares blob oids, so nothing is read from or written to the object database or disk. Clients that blindly re-write unchanged files cannot pollute history with empty commits.
- **Rename = single hook** — a `POST .../move` produces one `file.moved` event with both `from` and `to` paths, preserving entity identity in downstream systems.
- **Folder operations are answered from git trees, never the filesystem** — a folder is a `Tree` entry in HEAD's tree exactly as a file is a `Blob` one, so "does this folder exist" and "which files are under it" are both plain tree lookups. Nothing in the prefix-path feature touches the working tree to *decide* anything (it is still mirrored afterwards, best-effort, so humans can inspect repos), which keeps the HEAD-is-authoritative rule intact: a stray on-disk folder cannot cause a commit, and a folder missing on disk cannot prevent one.
- **Recursion is opt-in per request, and the flag only permits — never forces** — `allow_prefix_path_recurse` on delete/move (and `check_prefix_path` on the existence check) default to `false`, so a folder path keeps answering `404` for every caller that has not asked for the new behaviour. The delete/move flag travels in the **request body**, not the query string: it changes what the write does, so it belongs beside `author`/`message`/`destination`, and it keeps the rule that query parameters on this API only ever shape reads. `check_prefix_path` is a query parameter solely because `HEAD` carries no body. With the flag on, the route classifies the path against HEAD's tree *under the tenant write lock* (so the classification cannot go stale before the commit it drives) and dispatches to the single-file or the whole-folder operation; the classification lookup is only paid for when the flag is set. The two flag names differ deliberately: the existence check merely widens what counts as existing, whereas delete/move gain an unbounded, destructive mode, and the name should say so at the call site.
- **Folder delete/move are their own git functions, not modes** — `delete_directory`/`move_directory` sit beside `delete_file`/`move_file` rather than behind a boolean on them, mirroring the revert/rollback split: each function has one unambiguous scope and blast radius. The recursive delete removes a *single* directory entry from HEAD's tree (libgit2's tree updater drops the subtree with it and prunes parents left empty), so commit cost stays proportional to path depth rather than to the number of files removed. The recursive move reuses every blob oid verbatim, so no content is rehashed. In both, only the hook list scales with the file count — which is also the feature's real cost ceiling, since a moved folder's hook payloads hold each file's content in memory (the same exposure `rollback_commit` already carries for a large commit).
- **A folder rename stays a rename, file by file** — a recursive move emits one `file.moved` event per file rather than a delete/create wave, because each file keeps its leaf name and only its ancestor prefix changes. Downstream systems therefore keep whatever metadata they had attached to every file in the folder. The destination must not exist in any form and must not sit inside the source; the whitelist is skipped on a folder destination (it has no extension, and the leaf names it would guard are unchanged) but still enforced when the source turns out to be a file.
- **File order is data, stored per directory, and it is its own resource** — git offers nothing to hang an order off (trees are name-sorted, entries carry no metadata), so the order of a directory's entries is stored as a `.order.json` blob *in that directory*, holding leaf names only. Per-directory rather than one index at the repository root because ordering is inherently a sibling-level concern, and scoping the storage the same way bounds two costs: a reorder rewrites one small file instead of an O(all files) one on every change, and a recursive folder move needs zero index rewriting (leaf-name entries stay correct wherever the subtree lands, where full-path entries would all need their prefix swapped). It is exposed as `GET`/`PUT`/`DELETE /order[/*path]` rather than as a flag on the file routes — the same "separate routes, not mode flags" split as revert/rollback and `delete_file`/`delete_directory` — for three reasons that a flag cannot deliver: the format cannot be bypassed (the server owns the path, so there is no unvalidated way in), the stored spelling stays private (callers exchange a JSON array of names, not a blob), and a change delivers as a real `order.updated`/`order.deleted` event carrying a snapshot instead of a `file.updated` on a magic path the receiver would have to sniff and diff. Enforcing that split is why the index is **invisible to every `/files` route** — list, count, read, `HEAD`, batch — regardless of `include_hidden_files`; the dot prefix is Unix convention for metadata, not the mechanism.
- **One file's position is its own route, under `/files`** — `POST /files/*path/reorder` moves a single file to a numerical `position` in its parent's index, shifting the rest down. It exists beside `PUT /order` because the two answer different questions: a caller that knows the whole sequence sends it wholesale, while a caller that only knows where *one* file should go (a drag-and-drop) would otherwise have to read the index, splice it, and re-send it — three round trips racing every other writer, where this is one request settled under the write lock. It is addressed by the file being positioned rather than by the directory holding the index, which is why it lives under `/files` despite writing an order index; it is also why POST on a file path now dispatches on its suffix (`/move` or `/reorder`), with the suffix — never the body's shape — deciding which operation runs, so a mistyped field cannot silently turn one into the other. It positions **files by default and folders only on request** (`allow_prefix_path: true`), the third member of the prefix-path flag family and the one without a `_recurse` suffix: a folder's position is one entry in one index, so nothing recurses and nothing inside it changes — where delete and move gain an unbounded destructive mode, this gains one more addressable entry. Like its siblings the flag only permits, never forces: a file path behaves identically with it on, a folder path without it is just "not a file" and answers `404`, and the kind is decided from HEAD's tree under the write lock so a folder entry is stored in the canonical trailing-slash spelling. The index is **read and shifted, never rebuilt from the directory**: an unlisted sibling stays unlisted, because absence from the index is a state ("implicitly ordered"), not a gap to fill. The position counts against the index's *other* entries (the file is dropped from wherever it sat first, so re-requesting its current position is a genuine no-op), and a position past the end is clamped to the tail rather than rejected — the index is sparse, so its length is not something a caller can be expected to know. **`position: -1` is the same route's inverse**: it drops the entry from the index and leaves the file alone, so pinning and unpinning are one operation with one shape rather than two routes, and the read route's `position` becomes a round-trippable value — `-1` means "unlisted" in both directions, so a client can echo back what it read. Below `-1` is a `400`: positions are list indexes, and there is nothing else a negative one could mean. Everything else is the order routes' contract verbatim: an `order.updated` snapshot rather than a `file.*` event, no-op when nothing changes, and no `allowed_extensions` check since no path is being written.
- **Order writes are validated strictly, reads tolerate staleness** — every entry of a `PUT /order` must resolve inside that directory in HEAD's tree, canonicalised from what it resolved to (a directory gains a trailing slash, a file does not); an entry naming something absent is a `400`. Strictness is safe here precisely because the resolution happens under the tenant write lock that every write takes, so the classification cannot go stale before the commit it drives — the same reasoning as the `allow_prefix_path_recurse` path classification. The cost is that reorder-before-create is impossible (create the files, then order them). Reads take the opposite stance and ignore entries naming anything absent, because revert and rollback restore indexes out of history *without* validation (content coming from paths this server already committed, the same exemption `allowed_extensions` has there) and a stale index must never turn a listing into an error. A malformed index — only reachable by hand-editing a commit — degrades to "no index" for the same reason, and emits no hook rather than leaking as a file event.
- **An order change is a file change internally; the event kind is derived from the path** — every order operation returns an ordinary `FileChange` on the index's own path, and `HookJob::new` is the single place that recognises an order-index path and splits it out into an `OrderChange`. Classifying on the path rather than on the producing route is what makes every producer work with no special case: an explicit order write, a delete or move that rewrote an index alongside the file, a recursive folder operation carrying indexes inside its subtree, and a revert or rollback restoring an index out of history all arrive as plain file changes and leave correctly classified. Order events are delivered *after* every file event of the same commit, so an order snapshot never names a file the receiver has not been told about yet.
- **Order upkeep is implicit and rides in the same commit** — deleting or moving a file or folder rewrites the affected directory's index in the very commit that moved the file, so a downstream order table can never hold a position for something that is gone. A rename inside one directory replaces the entry **in place** (demoting a file to the tail for changing its name would silently reorder content the caller only renamed); a cross-directory move appends to the destination index *only when one already exists* (creating one would pin a single file in a directory whose siblings are all implicitly ordered); an index left empty is removed rather than stored empty. Creating a file adds nothing — a new file is unlisted, which means "at the tail", so indexes stay sparse by default.
- **Order events have no `moved` variant, deliberately** — a folder move relocates the indexes inside it, and `file.moved` exists precisely to preserve identity across a rename, yet a relocated index arrives as `order.deleted` at the old directory plus `order.updated` at the new one. The asymmetry is the point: a file is an entity a receiver hangs metadata off, whereas a directory's order list is just positions with no identity worth carrying, so a delete-plus-snapshot is both sufficient and one fewer event kind to support.
- **Replay repairs a drifted mirror in place, never by wiping it** — hook delivery is durable only in memory, so a receiver that was down past its retry budget (or that mis-applied an event) ends up holding a state this server never agreed to. The obvious repair — wipe the mirror and replay everything — is lossy whenever the mirror holds metadata the repository does not, and it also breaks referential integrity and blanks the tenant for the duration. `POST /batch/replay/hook` therefore repairs in place, and covers both directions of drift with one set operation rather than two routes: the caller sends the paths it holds, the server intersects them with what git holds, and `direction` picks the side — `delete` replays a `file.deleted` for everything *outside* the intersection (the mirror's orphans), `create` a `file.created` for everything *inside* it (rows the mirror is missing, or whose content went stale). One route because the two take identical inputs and differ only in that single set operation; splitting them would duplicate the whole path-validation and snapshot topology to express a boolean. Omitting `files` defaults it to everything git holds, which makes `create` the whole-scope reconciliation and makes `delete` empty by construction — git cannot be missing what it just listed — an asymmetry that falls out of the set operation rather than needing a branch.
- **The replay snapshot is maximal whenever the caller supplies a list** — `include_hidden_files` shapes only the *default* set used when `files` is omitted. When `files` is given, the git-side snapshot includes hidden files regardless of the flag, because the set operation needs git's side to be maximal: a file hidden from the snapshot would fall outside the intersection and, in the `delete` direction, replay a deletion for a file that is still there. `prefix_path` is a guard rail rather than a join for the same reason — paths stay repo-root-relative as they are everywhere else on this API, and an entry outside the prefix is a `400`, since it would otherwise fall outside the intersection for a reason having nothing to do with whether git holds it.
- **The `create` direction replays `file.created`, not `file.updated`** — the case a replay is usually run for is a row the receiver never got, and an `UPDATE` handler would silently do nothing for exactly those. The trade is the one assumption the feature makes about the receiver: since the replay set includes files the mirror already holds, `file.created` must be treated as insert-or-replace rather than a bare `INSERT`. This is also why there is no `update` direction — it would repair strictly less than `create` does.
- **A replay takes the tenant write lock even though it commits nothing** — reads on this API never lock, and a replay is a pure read, but it *enqueues*, and that is what changes the rule. Without the lock a PUT committing concurrently could have its `file.created` enqueued ahead of a replay whose snapshot predates it, and the receiver would apply a `file.deleted` for a path that had just been created — exactly the drift the feature exists to repair. Holding the lock across the snapshot and the enqueue makes the pair atomic with respect to commits, so queue order still equals the order this server accepted things. Nothing else about a write applies: no commit, no working-tree change, and maintenance is not armed.
- **A replay carries paths, not content, and resolves each file at delivery** — every other `HookJob` materialises its payloads at commit time, which is correct when the change set is one commit's worth. A whole-repository replay would pin the entire corpus in memory until the job drained, and a `delay_ms` throttle can stretch that to hours, so `HookJob` splits into a `Commit` source and a `Replay` one: the replay job holds only paths plus the repository location, and the delivery task reads content in small chunks (64 files) just before those files' POSTs. Resident memory is bounded by the chunk rather than the repository, the repository is opened once per chunk rather than once per file, and the content delivered is fresh at delivery rather than hours stale. A file that vanished between the snapshot and its delivery is skipped, not an error — its real `file.deleted` is already queued behind the replay, so the receiver still converges. Unreadable content (invalid UTF-8) is likewise skipped with a warning, unlike the batch read route where it is a hard `422`: a batch read answers a caller who asked for exactly those files, whereas a replay runs unattended over thousands and must not abort on one.
- **`delay_ms` throttles the receiver; it does not order anything** — delivery is already strictly sequential per repository (one consumer task awaiting every POST, retry and backoff before taking the next), so the parameter's only job is sparing a receiver from a sustained burst. It pauses *between* deliveries and never after the last one, and it is capped at 60 s per gap because a replay holds its repository's hook queue for `delay_ms × file_count` — an unbounded delay is an unbounded outage for every later commit's hooks. Going slower than the cap is a matter of several `prefix_path`-scoped passes, not a larger number.
- **Replayed payloads are marked, not renamed** — a replayed event keeps its ordinary event name (`file.created`, `file.deleted`) and its ordinary `[hooks] events` subscription, so the receiver's existing handler runs again with no new code and no new config; it gains exactly one field, `"replayed": true`, so the receiver can still log, meter, or guard on it. The field is added *only* to replayed payloads, leaving every live payload byte-for-byte unchanged. A distinct event kind would have been a cleaner signal and defeated the entire purpose. An order-index path in a caller's `files` list is rejected with a `400` for the mirror image of this reasoning: events are classified by path, so a `file.deleted` on an index path would arrive as an `order.deleted` and wipe a directory's stored order.
- **Revert = new commit** — reverts never rewrite history; they produce a new inverse commit and fire the appropriate hooks for each changed file.
- **Rollback is its own route, distinct from revert** — `POST /commits/:sha/rollback` restores the files `:sha` touched to the state they had *at* it; `POST /commits/:sha/revert` undoes what `:sha` did. Both derive their scope from the commit's own change set (so neither takes paths in its body) and differ only in which side of it is read — `:sha`'s tree vs `parent(:sha)`'s — but they are separate routes rather than one route with a mode flag, so each has one unambiguous meaning: revert undoes a change, rollback restores a point in time (discarding every later change to those same paths). Both are `POST`: like every write in this API they append a commit and never remove one. The rollback needs no parent commit, which is why rolling back *to* the initial commit is allowed where reverting it is not. Target states are staged onto current HEAD reusing existing blob oids — no rehash, no content read except the copy each hook payload needs — and, like an unchanged PUT, a path already holding its target state is skipped; when that leaves nothing to do, no commit is created, no hook fires, and maintenance is not armed.
- **Author identity is caller-supplied** — every write request requires an `author` object with `name` and `email`. Both are stored in the git commit and validated as non-empty.
- **Commit identifier is named `sha`** (not `sha1`) — future-proof against git's SHA-256 migration; matches the convention used by GitHub, GitLab, and Gitea.
- **`:sha` parameters accept hexadecimal only** — a full or abbreviated commit SHA (4–64 hex chars). Revspecs (`HEAD~1`, `master@{1}`, `:/pattern`) are rejected with `400` so git semantics never leak through the API and history-search DoS is impossible.
- **HEAD is authoritative, not the working tree** — existence checks for writes, deletes, and moves are answered from HEAD's tree, and every commit tree is derived from HEAD's tree plus the intended change. Leftover working-tree state from a previously failed operation can never change an operation's outcome or be silently swept into a later commit. Moved content is read from HEAD's blob, not from disk.
- **Commits are built with `TreeUpdateBuilder`, not the git index** — cost per write is proportional to the touched path depth, not the repository size, so large repos write as fast as small ones. Moves and reverts reuse existing blob oids (no content rehash). The working tree is still kept in sync with single-file fs operations so humans can inspect repos, and the on-disk index is refreshed to HEAD during maintenance so `git status` stays meaningful.
- **Background maintenance repacks and expires (pruning is opt-in)** — the first write to a repository arms a one-shot timer (`[maintenance] delay_secs`, default 24 h; `enabled = false` turns it off). When it fires, the pass takes the tenant write lock and, via libgit2 (no `git` binary needed): expires reflogs, writes one consolidated packfile, deletes all loose objects and superseded packs, refreshes the index, and clears the slot so the next write re-arms it. By default every object is carried over into the new pack, so maintenance can never destroy data; with `destructive_prune = true` only objects reachable from a ref are kept, permanently dropping orphaned garbage (e.g. blobs from writes that failed mid-operation). Commit history is safe in both modes — history is append-only, so every past file version (including versions of since-deleted files) stays reachable through its commit. Pruning needs no grace period because objects are only ever created under the same write lock the pass holds. The repack is skipped when the repo is already consolidated (no loose objects, ≤ 1 pack). Repos receiving no writes are never touched; the schedule is in-memory only and does not survive restarts. Deleting a tenant disarms its pending timer.
- **Ordered listing is opt-in, and the only listing mode that opens blobs** — `apply_order_index=true` orders every level of the listing by the stored index of the directory it belongs to, reading one small index per directory actually rendered; the default `false` keeps the blob-free contract and leaves every existing caller's results unchanged. Ordering is applied to the listing root *before* the page window is sliced, since pagination is over root-level entries, and only in-page subtrees are descended — so the off-page-directories-never-walked optimisation survives, unlike in name-search and date-filter mode. A depth-limited stub costs no index read. Listed entries come first in index order (files and directories interleaved freely, unlike the default directories-first rule), and unlisted entries keep their ordinary relative position, which is what makes a sparse index pin only what it names.
- **Where unlisted entries land is the caller's choice, not the index's** — an index names what should be pinned, so everything else is only ever "implicitly ordered", and *where* that implies is a presentation question the stored data has no business answering. `implicit_order_default_index` is therefore a listing parameter rather than a field in the index: it is the index unlisted entries are treated as holding, so the same stored order can render with the unordered tail on top (`0` or `-1`, the common case for "newest/unsorted first"), interleaved at any depth (`2`), or — unset, the default — behind everything listed, which is what every existing caller already gets. An unlisted entry sorts before a listed entry holding the same index, because otherwise `0` would mean "tied with the first" instead of "above it", and the whole point of the parameter is to lift them clear. Ranks are `i64` precisely so a negative value is expressible without a second parameter, and a directory with no index at all is left untouched: with nothing listed, a shared fallback index cannot reorder anything.
- **File listing never opens blobs** — the tree endpoint is served from git tree objects alone (no sizes are reported), and pagination over root-level entries is decided before any subtree is opened, so off-page directories are never walked. Two modes forgo the off-page optimisation because their matches can be nested anywhere, so they walk the whole in-scope tree before paginating over the *matched* tree's root-level entries: `file_name_starts_with` search (a case-insensitive prefix test on each entry's leaf name — files *and* directories, a matched directory carrying its whole subtree — with directories holding no match pruned), and the `include_date_from`/`include_date_to` date-range filter. Both still open no blob. The date filter, however, is the sole listing mode whose cost is *not* bounded by tree size: a tree entry carries no timestamp, so per-file created/updated dates come from a walk of commit history (blob-free — tree/oid deltas per commit, no patch, stats, or rename detection), whose cost scales with history length rather than page size. It is opt-in (inactive unless a bound is set), `updated` stops once every in-scope file is dated while `created` walks to the root, and it composes with the scoping parameters (files below a `maximum_depth` limit are not candidates; directories are kept only as structure leading to a surviving file). The count endpoint (`GET .../count/files`) keeps the pure tree-only contract: it walks tree objects only, honouring the listing's `prefix_path`/`maximum_depth`/`include_hidden_files` semantics, with an optional `restrict_file_extensions` filter matched against entry names (no date filtering).
- **Seek windowing lives in its own module (`seek.rs`) and scans, never decodes whole** — `SeekOptions` (query wire type, JSON-array strings since query parameters are strings) and `SeekBody` (JSON-body wire type for the batch route: native arrays, no `seek_` prefix) both parse into the validated `SeekFilter` the git layer consumes, through one shared funnel that turns every malformed value into a `400` naming the parameter as the caller spelled it. One canonical spelling per wire format — the only polymorphism, in both formats, is that the `to` filter accepts the bare `$seek_from_line_starts_with` operator unwrapped. The scan is a single forward pass over any `BufRead` that stops reading the moment the window is complete. The git layer feeds it a streaming ODB read (`git_odb_open_rstream`) when the blob is a loose object — every blob written since the last maintenance repack — so inflation halts early; packed objects cannot be streamed by libgit2 and fall back to a `Cursor` over the in-memory blob. Either way only the selected window is allocated (and only the window must be valid UTF-8 — prefix matching is byte-level). Seeked reads answer `404` when the path resolves to a folder, same as the HEAD existence endpoint.
- **Batch read is one repository pass** — the batch endpoint opens the repo and resolves HEAD's tree once, then reads every requested blob through the same windowed-read helper as the single route (streaming ODB read where possible). Results are index-aligned with the request; `null` strictly means "not found" (invalid-UTF-8 content fails the whole batch with 422 instead of masquerading as missing). All request-level validation — path sanitisation, uniqueness after sanitisation, the `limits.batch_read_maximum_files` cap (default 100), per-file seek resolution (an entry's own `seek` replaces the request-level one; validation errors name the offending entry as `files[i]: …`) — happens before the repository is touched. Reads never take the tenant write lock, so a large batch cannot stall writers.
- **Commit statistics are opt-in and page-scoped** — plain commit listing is served from commit metadata alone (no diffing), so `GET .../commits` stays cheap regardless of history size. `include_statistics=true` trades that for a real `diff_tree_to_tree` + rename-detection pass per commit, but only across the page window being returned (`per_page`, capped at 500), never the full history — the by-file listing variant defers this pass until after its match-and-paginate slice, so unpaginated matches are never diffed for nothing.
- **Per-tenant lock entries are never removed** — not even on tenant deletion. Removing an entry would let a writer holding the old mutex run concurrently with a writer holding a freshly-created one for the same repository.
- **Timestamps are named `committed_at`** — follows the `*_at` suffix convention (Stripe, GitHub API, Rails); unambiguous about what the value represents.
- **`/move` URL suffix on POST** — axum's wildcard router cannot match a fixed suffix after `*path`, so the handler is registered on `POST /*path` and enforces the `/move` suffix internally, returning 400 otherwise.
- **Stale `.git/index.lock` cleanup** — removed at startup across all repos, and before each maintenance index refresh (removed if older than 30 s). The write path itself no longer touches the index, so a stale lock can never block writes.
- **`git2` compiled with `vendored-libgit2`** — libgit2 is bundled in the binary; no system dependency needed.

## Webhook payloads

All payloads include `collection_id`, `tenant_id`, `commit_sha`, and `committed_at`.

`collection_id` and `tenant_id` together are the repository's identity, and together they are what a receiver must key its rows on. `tenant_id` alone is ambiguous — the same tenant id can exist under several collections, and those are separate repositories delivering on independent, separately-ordered queues.

**file.created / file.updated**
```json
{
  "event": "file.created",
  "collection_id": "docs",
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
  "collection_id": "docs",
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
  "collection_id": "docs",
  "tenant_id": "acme",
  "commit_sha": "b8d2e4a",
  "committed_at": "2026-06-16T10:01:00Z",
  "from": { "path": "docs/old.md" },
  "to": { "path": "docs/new.md", "content": "# Hello" }
}
```

**order.updated**
```json
{
  "event": "order.updated",
  "collection_id": "docs",
  "tenant_id": "acme",
  "commit_sha": "c4e1f7b",
  "committed_at": "2026-06-16T10:02:00Z",
  "directory": "docs/guides",
  "order": ["intro.md", "getting-started/", "advanced.mdx"]
}
```

**order.deleted**
```json
{
  "event": "order.deleted",
  "collection_id": "docs",
  "tenant_id": "acme",
  "commit_sha": "c4e1f7b",
  "committed_at": "2026-06-16T10:02:00Z",
  "directory": "docs/guides"
}
```

**Replayed events** carry one extra field, `"replayed": true`, and are otherwise identical to the live event of the same kind:
```json
{
  "event": "file.deleted",
  "collection_id": "docs",
  "tenant_id": "acme",
  "commit_sha": "a3f9c1d",
  "committed_at": "2026-06-16T10:00:00Z",
  "replayed": true,
  "file": { "path": "docs/removed.md" }
}
```

The field is present **only on replayed payloads** — a live event's payload stays byte-for-byte what it has always been, so nothing an existing receiver parses changes and its absence means "live". It is spelled as a past participle (`replayed`, not `replay`) because it states something about the event — "this event was replayed" — rather than instructing anything: the same distinction that makes request flags verb phrases (`include_hidden_files`, `apply_order_index`) and response state not (`has_more`). The event name is deliberately not varied: the whole point of a replay is that the receiver's existing handler runs again unmodified, with the flag there purely so it can log, meter, or guard on it. `commit_sha` is the HEAD the replay was snapshotted from rather than a commit that produced the change, and `committed_at` is when the replay was requested. Only `file.created` and `file.deleted` are ever replayed; there are no replayed `order.*`, `file.moved`, or `file.updated` events.

`directory` is repo-root-relative, with the repository root spelled as the empty string. `order.updated` carries the directory's **complete resulting order**, not a diff, so applying it downstream is a replace (`UPDATE … SET position = index`) and repeated delivery is harmless. `order.deleted` means that directory has no stored order any more and falls back to the default listing order.

Both are ordinary subscription entries in `[hooks] events`, so a receiver that does not list them gets no order events at all — which is what keeps the feature invisible to existing deployments.

### Delivery model

**One event per file, always.** There is no batching or coalescing anywhere: a commit's change set becomes one `HookJob`, and the consumer sends one HTTP POST per file change in it. So a recursive folder delete of *N* files produces one commit and *N* `file.deleted` events; a recursive folder move produces one commit and *N* `file.moved` events, each carrying that file's own `from`/`to` so entity identity survives. The same holds for reverts and rollbacks, which have carried multi-file change sets since they were added.

Per-file events are still subject to the `[hooks] events` subscription list — an event kind absent from that list is skipped, whether it came from a single-file or a recursive operation.

**Order events are delivered after every file event of the same commit.** A commit can carry both: deleting a file that its directory's index pins produces one `file.deleted` followed by one `order.updated` holding the index without it. Sending the file changes first means an order snapshot never references a file the receiver has not been told about yet. Recursive operations follow the same rule — a folder delete of *N* files whose subtree holds *M* indexes delivers *N* `file.deleted` events, then *M* `order.deleted` ones.

**Delivery is strictly sequential per repository, and concurrent across repositories.** The chain that guarantees it:

1. Jobs are enqueued *while the tenant write lock is still held*, so queue order equals commit order.
2. Each queue key gets exactly one `mpsc` sender and exactly one consumer task (`DashMap<String, UnboundedSender<HookJob>>`, created on first use).
3. That consumer `await`s each job to full completion — every file, every retry, every backoff sleep — before calling `recv()` again.
4. Within a job, files are `await`ed one at a time in change-set order.

Different keys are different tokio tasks, so they run concurrently — a slow or down receiver for one repository never delays another. The queue key is `"{collection_id}/{tenant_id}"`, the same composite key used for the write lock and maintenance slots, so all four subsystems agree on what "one repository" means.

The cost of this ordering guarantee is that latency is not bounded: a recursive delete of 10 000 files occupies that repository's queue for 10 000 sequential POSTs, and a receiver stuck in retries holds up every later commit for that repository. That is the intended trade — ordering beats latency, and a receiver applying events as they arrive always converges — but it is why recursion is opt-in per request.

Log lines from the delivery path name the repository as `repository="collection_id/tenant_id"` rather than by tenant alone, so the CRITICAL permanent-failure line identifies exactly which repository may now be out of sync.

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
