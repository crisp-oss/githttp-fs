Changelog
=========

## v1.11.4

### Changes

* Hook replays (`POST /v1/:collection_id/:tenant_id/batch/replay/hook`) now deliver their events in tree order, top down: a folder's own files first, then each of its sub-folders in turn, each finished before its next sibling begins, with hidden (dot-prefixed) entries leading inside each folder. A receiver is therefore never told about a file before the files of the folders above it. This holds in both directions and whatever order the `files` list was sent in; events were previously delivered in the order of that list, or in git's depth-first name order when it was omitted, which could deliver a deeply nested file before a file of one of its parent folders.
* The `order.updated` events closing a hook replay are now delivered top down as well: the repository root first, a directory before its sub-directories, hidden directories leading.
* Updated all dependencies to latest.

### Bug Fixes

* Fixed reads answering a raw `500` when they raced a tenant being created, deleted, or first synced on a replica: a repository now appears and disappears in one atomic step, and a read whose tenant is deleted while it runs answers `404`.
* Fixed a replica recording a repository's new head only after checking its working tree out, which left its listing — and any replica chained to it — behind its own reads for the length of that checkout, and could let a late sync record its head over a newer one.

## v1.11.3

### Changes

* Auto-generated commit messages for order writes (`PUT` and `DELETE` on `/order`, and `POST .../reorder`) now use the `reorder:` prefix instead of `order:`, and the reorder route no longer appends the entry's position (formerly `order: docs/intro.md -> 2` or `order: docs/intro.md -> unlisted`, now `reorder: docs/intro.md`).
* Auto-generated commit messages for file and folder moves now separate source and destination with `→` instead of `->`, e.g. `move: docs/old.md → docs/new.md`.

## v1.11.2

### Changes

* Auto-generated commit messages for order writes (`PUT` and `DELETE` on `/order`, and `POST .../reorder`) now all share the `order:` prefix, e.g. `order: docs/guides` or `order: docs/intro.md -> 2`, replacing the former `order update:`, `order delete:` and `order position:` prefixes.
* Updated all dependencies to latest.

## v1.11.1

### New Features

* Added the `server.checkout_files` configuration key (default `true`), which controls whether each tenant's files are kept on the working tree; turning it off saves the uncompressed size of all content, as nothing githttp-fs serves reads those files.
* Added the `server.checkout_files_autoheal` configuration key (default `false`), which checks every tenant out to its HEAD at startup, writing files that are missing on disk and removing files HEAD no longer names — what makes turning `checkout_files` back on retroactive.

### Bug Fixes

* A replica now mirrors its working tree like a master does: each landed pack checks the repository out to its new HEAD, so `ls` shows the same files on either node and `git status` is clean on both. A failed checkout is logged and never fails the sync.
* A replica restarting now reconnects its notification stream immediately instead of being refused for up to two heartbeats: the master releases a node id as soon as the peer's socket dies, and a `node_id_collision` issue is only raised once a refusal has persisted.

## v1.11.0

### New Features

* Added read-only replication: a node can run as a `master` (serving replicas) or a `replica` (following a master), configured with a new `[replication]` section. Replication is pull-based, replicas dial out to the master, can chain behind another replica, and never accept content writes (a write to a replica answers `423`).
* Added a dedicated replication HTTP server on its own port (default `5356`), guarded by `replication.secret` and serving `/_replication/{state,health,events,:collection_id/:tenant_id/pack}`.
* Added public (unauthenticated) health routes: `GET /v1/_health/status` reports the process name, version, role, writability and uptime with no I/O; `GET /v1/_health/replication` reports the replication picture (role, master health, every replica and its `sync` verdict, and an `issues` list) on every node, including standalone ones.
* Added a `replica` object to the `GET /v1` response on replicas, reporting bootstrap state, stream connectivity, last reconcile time, pending repositories and a one-word `sync` verdict.
* Added the `maximum_packs` maintenance option, which runs the maintenance pass immediately once a repository holds that many packfiles (meant for replicas).

### Changes

* A replica never destroys its own data on its own: a repository whose history is ahead of, or diverged from, its master's is kept, served and locked out of replication with a reported issue, and a master listing that would delete more than half of a replica's repositories is refused (`deletion_guard`).
* Every timestamp in an API body is now an RFC 3339 date-time (`2026-06-16T10:00:00Z`), including the new health and replication bodies.
* The shipped `config.toml` is now a standalone node (no `[replication]` section); `config.master.toml` and `config.replica.toml` are provided separately for running both roles side by side.

## v1.10.2

### Changes

* Hook replays now also re-dispatch one `order.updated` event per in-scope directory holding an order index, delivered after all file events, so a downstream order table is reconciled along with the files.
* Added Debian package installation instructions.

## v1.10.1

### Changes

* Auto-initialize the order index when changing the position of a file and the index is not yet initialized.
* Do not include hidden files by default in the order index (make it opt-in).

## v1.10.0

### New Features

* Added batch route to replay Web Hooks (allowing for full re-synchronization to target database).

## v1.9.1

### New Features

* Added `/reorder` route on files to re-order using a numerical position.

### Changes

* Return order position in read file route.

## v1.9.0

### New Features

* Added routes to manage file ordering in listings.

## v1.8.0

### New Features

* Added recursive file move, delete and directory exist check arguments.

### Changes

* Web Hook payloads now carry `collection_id` alongside `tenant_id`.

## v1.7.0

### New Features

* Added commit rollback route.

## v1.6.0

### New Features

* Added `include_date_from`, `include_date_to` and `include_date_type` options to file listing route.

### Changes

* `file_name_starts_with` now also accepts an array of prefixes.

## v1.5.1

### New Features

* Added `file_name_starts_with` option to file listing route.

## v1.5.0

### New Features

* Added changed files reporting in commits history routes.

## v1.4.0

### New Features

* Added file and directory count statistics route.

## v1.3.0

### New Features

* Added `include_hidden_files` option to file listing route.
* Added per-file seek overrides to batch file read route.

## v1.2.0

### New Features

* Implemented `seek_` options in the read file route, to filter returned content.
* Added batch file read route, to read multiple files at once (with support for the `seek` option).

## v1.1.1

### New Features

* Added route to ping server.

## v1.1.0

### New Features

* Added new option to maintenance configuration: `destructive_prune`.

### Changes

* Do not commit if a written file did not change.
* Improved background maintenance: repack, prune, reflog expiry and index refresh.

## v1.0.7

### Changes

* Security improvements.
* Performance improvements (for large repositories with lots of files, and/or deep nesting).

## v1.0.6

### New Features

* Added maximum depth argument to list files route.

## v1.0.5

### New Features

* Implemented route to check file existence.

## v1.0.4

### New Features

* Implemented listing of files at a directory prefix.
* Implemented listing of commits matching an exact file path.

## v1.0.3

### New Features

* Implemented collection nesting of per-tenant repositories.

### Changes

* Improved management of paged results.
* Updated all dependencies to latest.

## v1.0.2

### New Features

* Initial release.
