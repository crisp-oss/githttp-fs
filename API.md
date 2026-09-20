# githttp-fs — HTTP API and webhooks

The complete API surface: every route, request body, response shape, and webhook payload. The reasoning behind it lives in [DESIGN.md](DESIGN.md); working conventions live in [CLAUDE.md](CLAUDE.md).

## HTTP API

All routes are prefixed `/v1` and require `Authorization: Bearer <api_key>` — except the two `/v1/_health/*` routes, which are deliberately public (see [Public health routes](#public-health-routes)).

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/v1` | Check that the API key is valid (`200` with body `{ "pong": true }`, or `401`) |
| `GET` | `/v1/_health/status` | **No auth.** Basic server status: name, version, role, whether this node accepts writes, uptime |
| `GET` | `/v1/_health/replication` | **No auth.** Replication status: this node's role, the master's health, and every replica following it. Answers on every node, including a standalone one |
| `DELETE` | `/v1/:collection_id/:tenant_id` | Delete entire tenant repository |
| `GET` | `/v1/:collection_id/:tenant_id/files?prefix_path=&maximum_depth=&include_hidden_files=&file_name_starts_with=&include_date_from=&include_date_to=&include_date_type=&apply_order_index=&implicit_order_default_index=&page=&per_page=` | List tracked files as a tree; optional `apply_order_index` (default `false`) orders every level by that directory's stored file order index, and optional `implicit_order_default_index` (a number, unset by default) sets the index an entry the order index does not name is treated as holding — `0` or `-1` lifts every unordered entry *above* the ordered ones, unset leaves them behind them; optional `prefix_path` scopes the listing to a sub-directory (e.g. `?prefix_path=/docs`); optional `maximum_depth` limits how many directory levels deep the listing goes; optional `include_hidden_files` (default `false`) includes dot-prefixed entries; optional `file_name_starts_with` narrows the listing to files *and directories* whose leaf name begins with the given prefix, case-insensitively (a matched directory brings its contents along); it accepts either a bare string or a JSON-array string of prefixes (e.g. `?file_name_starts_with=["intro", "readme"]`), matching an entry whose name begins with *any* of them; optional `include_date_from`/`include_date_to` (RFC 3339 date-times) narrow the listing to files whose git date falls in the half-open window `[from, to)`, and `include_date_type` (`updated` default, or `created`) selects which date is compared; `page`/`per_page` paginate over the root-level entries of the listing (default 100, max 500) |
| `GET` | `/v1/:collection_id/:tenant_id/count/files?prefix_path=&maximum_depth=&include_hidden_files=&restrict_file_extensions=` | Count files and directories; `prefix_path`, `maximum_depth`, and `include_hidden_files` carry the same semantics as on the file listing route; optional `restrict_file_extensions` (a stringified JSON array, e.g. `["md", "mdx"]`) narrows the file count to files with one of those extensions |
| `GET` | `/v1/:collection_id/:tenant_id/files/*path?seek_from_line_starts_with=&seek_to_line_starts_with=&seek_lines_maximum=` | Read file content, plus its `position` in its parent directory's file order index (`-1` when unlisted); optional `seek_*` parameters narrow the response to a line window (see below) |
| `POST` | `/v1/:collection_id/:tenant_id/batch/files/read` | Batch-read several files in one request, with an optional shared seek window (overridable per file); capped by `limits.batch_read_maximum_files` |
| `HEAD` | `/v1/:collection_id/:tenant_id/files/*path?check_prefix_path=` | Check that a file exists (`200` or `404`, no body); optional `check_prefix_path` (default `false`) makes a folder at that path count as existing too |
| `PUT` | `/v1/:collection_id/:tenant_id/files/*path` | Create or update a file |
| `DELETE` | `/v1/:collection_id/:tenant_id/files/*path` | Delete a file; the optional body flag `allow_prefix_path_recurse` (default `false`) lets the path name a folder instead, deleting every file beneath it recursively in one commit |
| `POST` | `/v1/:collection_id/:tenant_id/files/*path/move` | Move / rename a file; the optional body flag `allow_prefix_path_recurse` (default `false`) lets the source name a folder instead, relocating its whole subtree in one commit |
| `POST` | `/v1/:collection_id/:tenant_id/files/*path/reorder` | Give the file the numerical `position` from the body inside its parent directory's file order index, shifting the entries at and after it down by one, or drop it from the index with `position: -1`; the index is first materialised over every entry the directory holds, so the position counts against what the caller sees, with the optional body number `implicit_order_default_index` saying where the not-yet-indexed siblings are folded in (same meaning as the listing parameter) and the optional body flag `implicit_allow_hidden_files` (default `false`, and required when the positioned entry is itself hidden) deciding whether hidden siblings are folded in at all; the optional body flag `allow_prefix_path` (default `false`) lets the path name a folder instead, positioning the folder itself among its siblings; commits and fires `order.updated` exactly as the order routes do |
| `GET` | `/v1/:collection_id/:tenant_id/order` and `/v1/:collection_id/:tenant_id/order/*path` | Read the file order stored for a directory (`/order` being the repository root); `404` when it has none |
| `PUT` | `/v1/:collection_id/:tenant_id/order` and `/v1/:collection_id/:tenant_id/order/*path` | Replace a directory's file order; hidden (dot-prefixed) entries are a `400` unless the body sets `allow_hidden_files: true` |
| `DELETE` | `/v1/:collection_id/:tenant_id/order` and `/v1/:collection_id/:tenant_id/order/*path` | Drop a directory's file order, reverting it to the default listing order |
| `POST` | `/v1/:collection_id/:tenant_id/batch/replay/hook` | Reconciliation: intersect the paths a downstream mirror holds with what this server holds, and replay one hook per file on the side `direction` selects — `delete` fires `file.deleted` for everything *outside* the intersection, `create` fires `file.created` for everything *inside* it. Every directory in scope that holds a file order index then gets one `order.updated`, after all file events. Optionally scoped and throttled. Commits nothing |
| `GET` | `/v1/:collection_id/:tenant_id/commits?page=&per_page=&file_path=&include_statistics=` | List commits, paginated (default 100, max 500); optional `file_path` filters to commits touching that file, following renames backward; optional `include_statistics` adds per-commit insertion/deletion/files-changed counts |
| `GET` | `/v1/:collection_id/:tenant_id/commits/:sha` | Commit detail with per-file diffs and snapshots |
| `POST` | `/v1/:collection_id/:tenant_id/commits/:sha/revert` | Revert a commit |
| `POST` | `/v1/:collection_id/:tenant_id/commits/:sha/rollback` | Roll the files that commit touched back to the state they had *at* it (point-in-time rollback) |

### Public health routes

`GET /v1/_health/status` and `GET /v1/_health/replication` are the only routes on the content API that take no `Authorization` header. They exist to be readable by things that do not hold — and should not need — the product's API key: a load balancer choosing between nodes, a rollout probe waiting for a new version to answer, a client discovering which node of a replicated set accepts writes, an operator dashboard covering a whole deployment. `GET /v1` remains the *authenticated* probe, and is still the way to verify that a key works.

Neither route names a tenant, opens a repository, or resolves a path, so the public surface stays metadata about the *process and the node set*, never about content. `status` additionally performs no I/O at all — every field is read from the config or from an atomic already in memory — so an anonymous caller cannot make the node do work by polling it. `replication` does describe topology (peer node ids, the master's URL with credentials stripped, the data-set identity, a repository count); none of it is a credential, but a deployment that treats internal hostnames as sensitive should keep the content port off the public internet.

They are nested *outside* both middleware layers rather than exempted inside them, so the API-key guard and the replica read-only guard never see them. A consequence worth having: a cold replica that is answering `503` to every content read still answers both of these, which is how an operator finds out why.

`_health` is the one collection id this API reserves. Only the two exact paths above are taken — `/v1/_health/{tenant_id}/...` still routes to the ordinary tenant routes — so the sole collision is a collection named `_health` holding a tenant named `status` or `replication`.

### Replication API (internal — a second server on its own port)

When `[replication]` is configured, githttp-fs binds a separate peer-only listener (default port `5356`) protected by `Authorization: Bearer <replication.secret>`. It serves `/_replication/{state,health,events,:collection_id/:tenant_id/pack}` and is exposed by masters and replicas so replicas can chain. A replica's `master_url` points to this listener, never the content API port.

`state` and `pack` provide convergence; `events` is a disposable latency hint and `health` is observability. Operators should use `GET /v1/_health/replication`, which serves the identical body without any credential.

See [REPLICATION.md](REPLICATION.md) for the complete peer protocol: authentication, wire formats, versioning, reconciliation, identity pairing, replica behavior, failure handling, and implementation constraints.

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
  "implicit_order_default_index": 0,
  "implicit_allow_hidden_files": false,
  "message": "optional commit message",
  "allow_prefix_path": false
}
```

`position` is required and must be a **number** — a zero-based index, so `0` puts the entry first; a non-number (a string, a fraction) is a `400`. It counts against the *whole parent directory*, not against the handful of siblings the index happens to name: the index is [materialised](#file-order-index) over every entry the directory holds before the move, so `position` means the row the caller was looking at. The entry is then dropped from wherever it sat and re-inserted at `position`, shifting the entries at and after it down by one. A position past the end is clamped to the tail rather than rejected — a caller cannot be expected to count the directory.

`implicit_order_default_index` is optional and says where the siblings the index does not name yet are folded in when it is materialised. It is the same number, with the same meaning, as the [listing parameter of that name](#response-shapes): unset leaves them behind everything the index lists, `0` (or any negative value) lifts them above it, `2` slots them between the index's second and third entries. A caller reordering inside a rendered listing passes back whatever it rendered with, and the index it gets is the sequence it was showing. It is inert with `position: -1`, which materialises nothing.

`implicit_allow_hidden_files` is optional and defaults to `false`, which keeps **hidden (dot-prefixed) siblings out of the materialised index** — the same judgement `include_hidden_files=false` makes on a listing, and the right default for an index the server generates from a directory rather than one the caller dictates entry by entry. A hidden entry the index *already* names is kept regardless: it was pinned deliberately, and positioning an unrelated file is no occasion to unpin it. The flag becomes **required** — as `true`, else `400` — when the entry being positioned is itself hidden, since pinning a dot-file has to be asked for. It is inert with `position: -1`: nothing is materialised there, and `-1` *unpins*, which is the guardrail's own direction and needs no opt-in (so a hidden entry pinned earlier can always be dropped without the flag).

`position: -1` is the one accepted negative value, and it is the inverse operation: the entry is dropped from the index and not re-inserted, leaving it implicitly ordered again. Nothing else changes — the file or folder itself is untouched, exactly as when it is merely moved, and the index is *not* materialised (unpinning one entry is no reason to pin every other one) — and if it was the index's last entry the index is removed rather than stored empty, so the event is `order.deleted` instead of `order.updated`. `-1` is deliberately the same value the [read route](#response-shapes) reports as `position` for an unlisted file: what a caller reads back is what it can send. Any value below `-1` is a `400`.

`allow_prefix_path` is optional and defaults to `false`: only a file is positionable, and a folder path answers `404` like any other "not a file". Set it to `true` and the path may name a folder too, which positions **the folder itself** among its siblings — an index interleaves files and directories freely, so a folder takes a slot exactly as a file does, and it is stored in the canonical spelling (with a trailing slash, which the caller may also use on the path). Like `allow_prefix_path_recurse` on delete and move, the flag only *permits* — a file path behaves identically with it on. Unlike those two it carries no `_recurse` suffix because nothing recurses: a folder's position is one entry in one index, and the folder's contents are untouched (indexes inside it keep their own order, since they order a different directory).

The index is **materialised over the whole directory** before the entry is moved: every sibling it does not name yet is folded in at the rank a listing renders it with (directories first, then alphabetical, placed by `implicit_order_default_index`), so what the caller positions against is the sequence they were shown rather than a sparse subset of it. Hidden (dot-prefixed) siblings are **not** folded in unless `implicit_allow_hidden_files: true` says so, and that flag is required outright when the entry being positioned is itself hidden; one the index already names is kept either way. A parent with no index yet gets one covering the directory — unlike the implicit upkeep that never creates an index, this is an explicit request for a position, exactly as `PUT /order` is; the price is that the first reorder in a directory pins all of its entries, which is precisely what makes the second one land where the caller expects. Entries the stale index names but the directory no longer holds are dropped in passing (reads stay tolerant of them, but there is no reason to write one back). The entry must exist in the last committed state (`404` otherwise, `-1` included — dropping the position of something that is gone is a caller bug, and implicit upkeep already handles the real deletion); the path is classified against HEAD's tree under the tenant write lock, so it cannot go stale before the commit it drives. `limits.allowed_extensions` does not apply (no path is being written). Asking for the state the index already holds is a no-op — no commit, no hook, HEAD's sha — in both directions: a request whose materialised result is byte-for-byte the stored index (an entry already at `position` in an index that already covers its directory), and an already-unlisted entry sent `-1` (including when the directory has no index at all).

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
  "allow_hidden_files": false,
  "message": "optional commit message"
}
```

`order` is required and must hold at least one entry (an empty order is a `400` — that is what `DELETE` is for). Entries are **leaf names**, not paths: a nested path, `.`, `..`, `.git`, an empty name, a duplicate (after any trailing slash is stripped), or a reference to the index file itself are all `400`. A trailing slash marking a directory is accepted and normalised — the server stores directories with one and files without, whichever spelling the caller used.

`allow_hidden_files` is optional and defaults to `false`, which makes a **hidden (dot-prefixed) entry a `400`** — an index is a presentation order, and a dot-file is by convention not presented, so pinning one has to be asked for. Rejected rather than silently dropped: the caller sent a name and a position for it, and storing a different order than the one they wrote would be worse than telling them the rule. Set it to `true` and hidden entries are ordinary entries, subject to every other rule unchanged. The index file itself stays a `400` either way — the flag opens hidden *entries*, not the one path that is not an entry at all.

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

`include_hidden_files` (default `false`) is only meaningful **when `files` is omitted**, where it shapes the default set exactly as on the listing route. When `files` *is* given, the git-side snapshot always includes hidden files no matter what the flag says, because the set operation needs git's set to be maximal: a file hidden from the snapshot would fall outside the intersection and replay a `file.deleted` for a file that is very much still there. Order indexes are excluded from the *file* snapshot either way, exactly as they are from every `/files` route — an index is never replayed as a file. It shapes the order phase too, but on directories rather than files: a hidden directory is pruned unless hidden files are included, so its index is not replayed either. Neither `file.updated`, `file.moved`, nor `order.deleted` is ever replayed.

**Events are replayed in tree order, top down — as if a user were creating the repository by hand, level by level, like peeling an onion.** A folder's own files are delivered first, and only once that folder is complete are its sub-folders opened, one after the other, each under the same rule and each finished before its next sibling begins. Inside one folder, hidden (dot-prefixed) entries lead — files among files, folders among folders — and names then compare bytewise, as the listing sorts them. For this tree:

```
.meta.md
-notes.md
README.md
zed.md
docs/
  .draft.md
  intro.md
  guides/
    setup.md
legal/
  terms.md
```

the events arrive as `.meta.md`, `-notes.md`, `README.md`, `zed.md` (the root is now complete), then `docs/.draft.md`, `docs/intro.md` (`docs/` is complete), then `docs/guides/setup.md`, and only then `legal/terms.md`. So when a receiver is told about a file, every file sitting directly in each of its ancestor folders has already been delivered, and it never has to buffer or re-sort. This is a **contract**, in both directions and whatever order the caller listed its `files` in — that list's own order is ignored. `prefix_path` scopes the same sequence to one subtree, and the order phase below lists its directories the same way: the root first, a parent before its children, hidden folders leading.

**Order indexes are replayed last, unconditionally.** After every file event of the replay, one `order.updated` fires for each directory in scope that holds a [file order index](#file-order-index), carrying that directory's complete stored order. It is not intersected with `files` and takes no flag of its own: an index belongs to a *directory*, not to a file, so there is nothing in the caller's list to intersect it with, and a snapshot is idempotent — replaying an order the mirror already has correct changes nothing. Only `order.updated` is replayed: a replay states what the repository holds, and a directory with no index holds nothing to state (an `order.deleted` would have to be replayed for every directory *without* an index, which is every directory in most repositories). The phase is subscription-gated on its own, so a receiver that does not list `order.updated` in `[hooks] events` gets none of them and its file replay is unaffected — and, symmetrically, a receiver subscribed to `order.updated` but not to the file event still gets its order snapshots. `delay_ms` throttles these deliveries exactly as it does the file ones, continuing the same gap sequence rather than restarting it.

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

On a **replica** the body gains a `replica` object, and only there — a master and a standalone node answer exactly the body above, as they always have:
```json
{
  "pong": true,
  "replica": {
    "state": "ready",
    "stream_connected": true,
    "last_reconcile_at": "2026-06-16T10:00:00Z",
    "pending_repositories": 0,
    "sync": "synced"
  }
}
```

`state` is `"ready"` or `"bootstrapping"`; `stream_connected` says whether the live notification channel to the master is up (`false` means this node is converging on its poll interval alone); `last_reconcile_at` is when it last compared its whole repository set against the master (`null` before the first pass); `pending_repositories` counts repositories known to be behind; `sync` is the one-word verdict on whether following is keeping up and will on its own — `"synced"`, `"lagging"`, `"stalled"` (heals when whatever is failing stops), or `"halted"` (waiting on a human; `GET /v1/_health/replication` lists what for under `issues`). This is the one route the replica read-only guard always lets through, so a node refusing every other request can still explain why.

Any request to the bare server root `/` (any method, no auth required) is answered with a `308 Permanent Redirect` to `/v1`.

**GET** `/v1/_health/status` — basic server status (no auth)
```json
{
  "status": "healthy",
  "name": "githttp-fs",
  "version": "1.11.0",
  "role": "master",
  "writable": true,
  "started_at": "2026-06-16T10:00:00Z",
  "uptime_secs": 4210
}
```

`status` is `"healthy"`, or `"bootstrapping"` on a replica that holds no content yet and is therefore refusing content reads. `role` is `"master"`, `"replica"`, or `"standalone"` (no `[replication]` section) — the same three values the replication status reports. `writable` says whether a write sent to this node would be accepted at all, so a failover-aware client can read it once instead of learning it from a `423`. `started_at` is an RFC 3339 date-time, like every timestamp on this API, and `uptime_secs` is the seconds since.

The response is always `200`, including while bootstrapping: the status code answers "is this process alive enough to reply", and *what* it can serve is the body's job to say. Collapsing the two would force a caller that only wants the version to interpret a `503`, and would hide the one state a bootstrapping node most needs to report.

**GET** `/v1/_health/replication` — replication status (no auth)

Byte-identical to what peers read from `/_replication/health` with the replication secret — one struct, one builder, two doors. See [REPLICATION.md](REPLICATION.md) for the full schema.

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
- **Recursive move** — the folder's whole subtree is relocated under `destination`. Every file keeps its own leaf name (only the ancestor prefix changes), so one `file.moved` hook fires per file and downstream entity identity survives the move. Blob oids are reused verbatim (no content rehash); content is read once per file purely to fill that file's hook payload, exactly as the single-file move does. Auto-generated message: `"move: docs/guides/ → docs/handbook/"`. The `destination` must not exist in any form (file or folder — the caller must delete it first) and must not sit *inside* the source, which would ask the folder to be moved into itself; both are a `400`.

The `limits.allowed_extensions` whitelist is not applied to a folder `destination` — it carries no extension of its own, and every file inside keeps its leaf name, so extensions are preserved by construction. It still applies normally when the source turns out to be a file, even with the parameter on (the check is simply deferred until the source kind is known).

A folder holding nothing this API can represent (no blobs at all — only submodule entries) is a no-op on both routes: no commit, no hook, and `commit_sha` is current HEAD, same contract as an unchanged PUT.

The repository root is not addressable: an empty path (`/` after sanitisation) is a `400` on both routes. Deleting everything remains the tenant route's job.

The reorder route takes a third flag of this family, `allow_prefix_path`, spelled without `_recurse` on purpose: it lets the path name a folder, but a folder's *position* is one entry in one index — nothing recurses, nothing is destroyed, and the folder's contents are untouched. Everything else is identical: a body field, opt-in, classified against HEAD under the write lock, permitting folder semantics rather than forcing them, and tolerating a trailing slash when on.

### File order index

Git has no ordering of its own — tree entries are name-sorted by definition and carry no metadata slot — so a presentation order has to be stored as data. githttp-fs stores it **per directory**: one index holding the leaf names of that directory's entries, in the order they should be presented. Ordering is a sibling-level concern, and scoping the storage the same way keeps two costs bounded: a reorder touches one small file (so its commit and its hook payload are proportional to one directory, not to the repository), and a folder move needs no index rewriting at all (entries are leaf names, so every index inside a relocated subtree is still correct once it travels with it).

The whole feature is optional. A repository with no index anywhere behaves exactly as before, and a caller that never passes `apply_order_index=true` and never subscribes to the `order.*` hook events cannot tell the feature exists.

**The index is a separate resource, never a file.** It is stored as a `.order.json` blob in the directory it orders, but that is an implementation detail on the same footing as git itself: the index is **invisible to every `/files` route** — listing, count, read, `HEAD`, and batch (where it comes back as `null`) — regardless of `include_hidden_files`, and the write routes refuse the path outright (`PUT`, and a move destination, answer `400` pointing at `/order`; a move source or a `DELETE` answers `404`, since to those it is simply not a file). That invisibility is what makes the format impossible to bypass: were the index an ordinary file, a client could `PUT` it directly and store anything, and a receiver would see a `file.updated` on a magic path it had to sniff, parse and diff instead of a real event.

**Two ways in.** `PUT /order[/*path]` replaces a whole directory's order at once — the bulk spelling, for a caller that knows the full sequence it wants. `POST /files/*path/reorder` moves *one* file to a numerical position inside its parent's index, shifting the rest down — the incremental spelling, for a caller that only knows where one thing should go (a drag-and-drop, say) and does not want to read, splice and re-send the whole list. Both go through the same validation, produce the same kind of commit, and deliver the same `order.updated` snapshot; the reorder route lives under `/files` because it is addressed by the *entry* being positioned, not by the directory holding the index. The incremental spelling **materialises the index over the whole directory** first (folding the not-yet-listed siblings in at `implicit_order_default_index`, hidden ones only with `implicit_allow_hidden_files`), because a position is only meaningful against what the caller was looking at. It positions files only unless the caller sets `allow_prefix_path: true`, which lets it position a folder among its siblings as well (an index interleaves the two freely).

**Hidden files stay out by default.** An index is a *presentation* order, and a dot-file is by convention not presented, so pinning one is noise a caller has to have asked for — the same judgement the listing's `include_hidden_files=false` makes. Each way in enforces it on its own terms, with its own opt-in, because they differ in who chose the names: `PUT /order` **rejects** a hidden entry (`400`) unless the body sets `allow_hidden_files: true`, since the caller named it explicitly and silently dropping it would store an order they did not write; a reorder **omits** hidden siblings from the index it materialises unless the body sets `implicit_allow_hidden_files: true`, since there the server picked the names and leaving one out is not a contradiction of anything the caller said. That second flag is required, as `true`, when the entry being positioned is itself hidden — it decides both the siblings' fate and whether a dot-file may be pinned at all, so one request carries one answer. A hidden entry an index already names is never dropped by a reorder for an unrelated entry, and `position: -1` unpins one without any flag: the guardrail keeps dot-files out, it does not trap them in.

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
| Reorder a file or folder (`POST .../reorder`) | The index is materialised over the whole directory (hidden entries excluded unless `implicit_allow_hidden_files: true`), then the entry is re-inserted at the requested position, shifting the entries at and after it down — or, with `position: -1`, dropped from the index (nothing materialised) while the file itself stays; unlike the rows above it *creates* the index when the parent has none, since the caller asked for a position explicitly (a folder needs `allow_prefix_path: true`) |

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
  "files": 12,
  "orders": 3
}
```

`files` is the number of files the batch affected: how many `file.deleted` hooks will be replayed in the `delete` direction, or how many `file.created` ones in the `create` direction. `0` means the two sides already agreed on that side of the intersection.

`orders` is the number of `order.updated` hooks that will follow them — one per directory in scope holding an order index, independent of `files` and of `direction`. Nothing is enqueued only when *both* counts are `0`.

The affected paths themselves are not echoed: in the `create` direction the set is bounded by the repository rather than by the request, so a whole-tenant replay would answer with the entire file list for no benefit. A caller that wants to see it can ask `GET /files`.

`commit_sha` is the HEAD the snapshot was taken from — no commit was created; it is the honest answer to "which state was this computed against".

The response returns as soon as the job is enqueued — delivery happens on the repository's hook queue afterwards, so a `200` means "scheduled", not "delivered". Progress and completion are visible in the logs (`replay starting` / `replay finished`, with delivered and skipped counts for the file phase and the order phase separately).

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

### Replication

Replication is a single-writer, pull-based Git-object replication system. `GET /v1/_health/replication` is the operator-facing status endpoint — public, like its `status` sibling — and is available on master, replica, and standalone nodes. Its top-level `status` (`healthy` / `degraded` / `halted`) and `issues` list are what an alert reads; its replica-only `replica` object explains bootstrap progress, stream connectivity, pending and locked repositories, and the follower's `sync` verdict.

A replica **never destroys its own data on its own**. A repository whose history is ahead of, or diverged from, its master's is kept, served, locked out of replication, and reported as an issue; a listing that would delete more than half of a replica's repositories is refused and reported. Recovery is an operator's decision — see the runbook in [REPLICATION.md](REPLICATION.md).

**Promotion — making a replica the master — is a config swap.** Only the master accepts writes, so when it is lost (a crashed process, a dead host, a planned retirement) the deployment can still *read* from every replica but has nowhere to write until one of them takes the write role. That hand-over is what promotion means, and it is deliberately manual and never automatic: two nodes accepting writes at once would fork two histories, and nothing here merges a fork. Any replica becomes the master by setting `role = "master"`, removing `master_url`, adding `[hooks]`, and restarting: its repositories are complete object stores and its `.replication.json` already holds the data-set identity, so the other replicas follow it by changing `master_url` alone. The step-by-step runbook, including how the old master comes back and how lost writes surface as `replica_ahead` issues, is in [REPLICATION.md](REPLICATION.md#promotion-making-a-replica-the-master).

The complete status schema, peer endpoints, identity pairing, reconciliation algorithm, replication safety properties, and operational failure modes are documented in [REPLICATION.md](REPLICATION.md).

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

The field is present **only on replayed payloads** — a live event's payload stays byte-for-byte what it has always been, so nothing an existing receiver parses changes and its absence means "live". It is spelled as a past participle (`replayed`, not `replay`) because it states something about the event — "this event was replayed" — rather than instructing anything: the same distinction that makes request flags verb phrases (`include_hidden_files`, `apply_order_index`) and response state not (`has_more`). The event name is deliberately not varied: the whole point of a replay is that the receiver's existing handler runs again unmodified, with the flag there purely so it can log, meter, or guard on it. `commit_sha` is the HEAD the replay was snapshotted from rather than a commit that produced the change, and `committed_at` is when the replay was requested. `file.created`, `file.deleted` and `order.updated` are the events a replay emits; there are no replayed `order.deleted`, `file.moved`, or `file.updated` events. A replayed `order.updated` carries the same `directory` and complete `order` a live one does, plus the flag, and every one of them is delivered after all of the replay's file events.

`directory` is repo-root-relative, with the repository root spelled as the empty string. `order.updated` carries the directory's **complete resulting order**, not a diff, so applying it downstream is a replace (`UPDATE … SET position = index`) and repeated delivery is harmless. `order.deleted` means that directory has no stored order any more and falls back to the default listing order.

Both are ordinary subscription entries in `[hooks] events`, so a receiver that does not list them gets no order events at all — which is what keeps the feature invisible to existing deployments.

### Delivery model

**One event per file, always.** There is no batching or coalescing anywhere: a commit's change set becomes one `HookJob`, and the consumer sends one HTTP POST per file change in it. So a recursive folder delete of *N* files produces one commit and *N* `file.deleted` events; a recursive folder move produces one commit and *N* `file.moved` events, each carrying that file's own `from`/`to` so entity identity survives. The same holds for reverts and rollbacks, which have carried multi-file change sets since they were added.

Per-file events are still subject to the `[hooks] events` subscription list — an event kind absent from that list is skipped, whether it came from a single-file or a recursive operation.

**Order events are delivered after every file event of the same commit.** A commit can carry both: deleting a file that its directory's index pins produces one `file.deleted` followed by one `order.updated` holding the index without it. Sending the file changes first means an order snapshot never references a file the receiver has not been told about yet. Recursive operations follow the same rule — a folder delete of *N* files whose subtree holds *M* indexes delivers *N* `file.deleted` events, then *M* `order.deleted` ones. A hook replay follows it too: its file events come first — in [tree order](#request-bodies), a folder's files before its sub-folders — then one `order.updated` per in-scope directory holding an index, parents before children.

**Delivery is strictly sequential per repository, and concurrent across repositories.** The chain that guarantees it:

1. Jobs are enqueued *while the tenant write lock is still held*, so queue order equals commit order.
2. Each queue key gets exactly one `mpsc` sender and exactly one consumer task (`DashMap<String, UnboundedSender<HookJob>>`, created on first use).
3. That consumer `await`s each job to full completion — every file, every retry, every backoff sleep — before calling `recv()` again.
4. Within a job, files are `await`ed one at a time in change-set order.

Different keys are different tokio tasks, so they run concurrently — a slow or down receiver for one repository never delays another. The queue key is `"{collection_id}/{tenant_id}"`, the same composite key used for the write lock and maintenance slots, so all four subsystems agree on what "one repository" means.

The cost of this ordering guarantee is that latency is not bounded: a recursive delete of 10 000 files occupies that repository's queue for 10 000 sequential POSTs, and a receiver stuck in retries holds up every later commit for that repository. That is the intended trade — ordering beats latency, and a receiver applying events as they arrive always converges — but it is why recursion is opt-in per request.

Log lines from the delivery path name the repository as `repository="collection_id/tenant_id"` rather than by tenant alone, so the CRITICAL permanent-failure line identifies exactly which repository may now be out of sync.
