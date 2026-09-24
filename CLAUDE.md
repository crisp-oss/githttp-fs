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
  traverse.rs      — the order a hook replay delivers its paths in (a folder's files, then its sub-folders)
  replication.rs   — read-only replication: data-set identity, repository index, change notifier, replica follower, catch-up
  checkout.rs      — startup healing of working trees that do not match HEAD (`checkout_files_autoheal`)
  routes/
    mod.rs         — shared request types (AuthorRequest)
    files.rs       — GET/PUT/DELETE/POST on /:collection_id/:tenant_id/files and /:collection_id/:tenant_id/files/*path (POST dispatching on the /move and /reorder suffixes), plus POST /:collection_id/:tenant_id/batch/files/read and GET /:collection_id/:tenant_id/count/files
    order.rs       — GET/PUT/DELETE on /:collection_id/:tenant_id/order and /:collection_id/:tenant_id/order/*path
    replay.rs      — POST /:collection_id/:tenant_id/batch/replay/hook (webhook replay for downstream reconciliation)
    health.rs      — public (unauthenticated) GET /_health/{status,replication}
    replication.rs — GET /_replication/{state,health,events,:collection_id/:tenant_id/pack}
    commits.rs     — commit list, commit detail, revert / point-in-time rollback
    tenant.rs      — DELETE /:collection_id/:tenant_id
```

## Where the detail lives

This file is loaded into every session, so it holds only what every task needs. Read the relevant file **before** touching the matching code:

- **[API.md](API.md)** — every route, query parameter, request body, response shape and webhook payload, plus the hook delivery model. Read it before changing anything under `src/routes/`, `hooks.rs`, `order.rs`, `seek.rs` or `traverse.rs`.
- **[DESIGN.md](DESIGN.md)** — the annotated `config.toml` reference and the rationale for every design decision (locking, HEAD-is-authoritative, `TreeUpdateBuilder` commits, order index, replay, maintenance, replication safety, working tree, staging renames). Read it before changing `git.rs`, `state.rs`, `replication.rs`, `checkout.rs` or `config.rs`, and before proposing a design that departs from an existing one.
- **[REPLICATION.md](REPLICATION.md)** — the peer protocol. **[CONSIDERATIONS.md](CONSIDERATIONS.md)** — operator-facing notes.

Rules that hold everywhere, in one line each:

- All routes are under `/v1` behind `Authorization: Bearer <api_key>`; only `GET /v1/_health/{status,replication}` are public, nested outside both middleware layers.
- Git never appears in the API surface; `:sha` accepts hexadecimal only.
- HEAD's tree is authoritative, never the working tree; commits are built with `TreeUpdateBuilder`, not the index.
- Writes take the per-tenant mutex (`"collection_id/tenant_id"`), run in `spawn_blocking`, and enqueue their hook job while the lock is held; reads never lock. Lock entries are never removed.
- An unchanged write is a no-op: no commit, no hook, HEAD's sha in the response.
- One hook per file, never batched; strictly ordered per repository, concurrent across repositories; order events after file events.
- Separate routes and git functions rather than mode flags (revert/rollback, `delete_file`/`delete_directory`, `/order` vs `/files`); body flags shape writes, query parameters shape reads; opt-in flags permit, never force.
- The `.order.json` index is invisible to every `/files` route; order writes validate strictly against HEAD, reads tolerate staleness.
- A replica never destroys its own data on its own: it locks the repository, raises an issue, and waits for a human.
- A new default must preserve what a deployment already did; bulk or destructive behaviour is opt-in.
- Every timestamp on the wire is RFC 3339 and named `*_at`.

## Running

```sh
cargo run                                    # uses config.toml in cwd
cargo run -- -c /etc/githttp-fs.toml
RUST_LOG=debug cargo run                     # overrides log_level in config
```

`config.toml` is a **standalone** node (no `[replication]` section) and is the file the Docker image and the Debian package install as `/etc/githttp-fs.toml`, so the shipped default opens no replication listener and holds no replication secret.

### Running a master and a replica side by side

The repo ships two further dev configs so both roles can run at once on one machine:

```sh
cargo run -- -c config.master.toml           # master:  api :5355, replication :5356
cargo run -- -c config.replica.toml          # replica: api :5365, replication :5366
```

Each node has its own store — `dev/repositories/master` and
`dev/repositories/replica` — because they are separate copies of the same
content, not two processes sharing a directory. Both are tracked by a
`.gitkeep` with everything inside them gitignored. They share
`server.api_key`, so one client credential reads from either node, which is
what makes failover testable: point a client at `:5365` and kill the master.

```sh
# Write to the master, read it back from the replica a moment later
curl -X PUT localhost:5355/v1/docs/acme/files/intro.md \
  -H 'Authorization: Bearer MySecretAPIKey' -H 'Content-Type: application/json' \
  -d '{"author":{"name":"V","email":"v@example.com"},"content":"# Hello"}'
curl localhost:5365/v1/docs/acme/files/intro.md -H 'Authorization: Bearer MySecretAPIKey'

# Who is following whom, and how far behind (no credential needed)
curl localhost:5355/v1/_health/replication

# What each node is running, and which of them takes writes
curl localhost:5355/v1/_health/status
curl localhost:5365/v1/_health/status

# Writes to the replica answer 423
curl -X PUT localhost:5365/v1/docs/acme/files/x.md -H 'Authorization: Bearer MySecretAPIKey' \
  -H 'Content-Type: application/json' -d '{"author":{"name":"V","email":"v@example.com"},"content":"x"}'
```

The replica config carries only what a read-only node actually reads, so the
two files are not near-copies of each other. It sets `poll_interval_secs = 10`
(rather than the default 60) so convergence is visible while developing, and
`destructive_prune = true`, which is safe on a replica since anything dropped
can be fetched again. Three things are deliberately absent:

- **`[hooks]`** — hook delivery belongs to the node that accepted the commit,
  and nothing on a replica enqueues one.
- **`limits.allowed_extensions`** — read only on file writes and move
  destinations, all of which answer `423` on a replica before a handler runs.
- **a second `api_key`** — it shares the master's, so one client credential
  reads from either node.

`limits.batch_read_maximum_files` *is* kept, because `POST /batch/files/read`
is the one write-shaped route a replica serves and that key caps it — a useful
reminder that "write-shaped" and "write" are different questions on this API.

## Development workflow

After applying code changes, always run `cargo fmt` before `cargo build`.

### Commit messages

**One line, and only one line.** A commit message is a single subject line with no body: the subject says what the change does, and why it is the way it is belongs in `DESIGN.md` or in `CONSIDERATIONS.md`, where it stays discoverable long after the commit has scrolled out of `git log`. The only things that ever follow the subject are the trailers a tool appends (`Co-Authored-By:`, `Claude-Session:`) and a merge commit's generated message.

The subject is written in the **imperative mood**, capitalised, with no trailing period — `Add check file exist route`, `Fix reads racing tenant creation, deletion and replica sync`, `Only auto-run tests prior to releasing`. There is no `type:` prefix and no scope: this repository does not use Conventional Commits, and [the changelog is written by hand at release time](#changelog) rather than derived from commit subjects, so a machine-readable prefix would buy nothing while a `feat:`/`fix:` split would have to be maintained for no reader.

The recurring verbs carry their ordinary meaning: `Add` and `Implement` for new surface, `Fix` for behaviour that did not match its documentation, `Update` and `Bump` for dependencies and generated files, `Normalize` / `Rename` / `Move` for mechanical passes. A change that genuinely does two things joins them with ` + ` rather than reaching for a vaguer summary — `Add configuration to tune the auto-checkout behavior of githttp-fs + auto-heal behavior`.

**A release commit is the version and nothing else** — `v1.11.3`, exactly as step 8 of the [release procedure](#release-procedure) says. Everything that went into it is already written up in `CHANGELOG.md` under that same heading, so a prose subject would only paraphrase it.

### Tests

The suite lives in `src/tests/`, registered as `#[cfg(test)] mod tests;` in `main.rs` rather than in a `tests/` directory — a binary crate has no library target, and an in-crate module reaches `build_router` and every internal module without adding one. Run it with `cargo test`.

**When to run it.** `cargo test` is **not** part of the ordinary edit loop: while building a feature or a fix, the routine after changing code stays `cargo fmt` then `cargo build`, because the build is what catches the mistakes worth catching at that stage and the suite is too slow to pay for on every iteration. It becomes mandatory at exactly one moment — **cutting a release** (see [Release procedure](#release-procedure) below), where it runs automatically and a failure blocks the release rather than being noted and worked around; the [end-to-end suite](#end-to-end-tests) runs there too, as the step straight after it. The exception is when the change under development *is* the suite itself, or when running it has been asked for: there, running it is the work rather than overhead.

It is split the way the codebase is. Modules that are pure functions of their input are tested directly (`validate`, `seek`, `order_index`, `config`, `util`, `traverse`); everything whose contract is an HTTP contract is tested through a **real server on a real socket** (`harness.rs`), because the API-key guard, the replica read-only guard, and the `/v1` nesting are router behaviour that a handler-level test would assert nothing about. Each server gets its own `tempfile::TempDir` as `repos_path`, so tests share no state and run fully parallel; hook tests drive a stub receiver that records payloads in delivery order — and that can refuse, stall one repository, or hold a delivery open — which is how the ordering, retry, and per-repository concurrency promises are asserted.

Replication is tested as a **real master/replica pair**, both listeners bound as `main` binds them, converging over the peer protocol. The safety properties are the point of that module's tests: a replica that holds history its master lacks is locked and kept rather than wiped, a listing that would delete most of a replica is refused, and an operator removing the local copy is the exit from a lock. Two kinds of fixture exist for states the API deliberately cannot produce — `fixture.rs` commits with explicit dates and non-UTF-8 blobs (for the date-range filter and the `422` path), and the replication tests commit directly into a store to fork its history.

The behaviour asserted is the behaviour documented in `API.md`. When a test's expectation looks surprising, the rule it comes from is quoted in a comment above it — so a change that breaks a test can be judged against what the API promised, not merely against what the code used to do. A new route, parameter, or event therefore belongs in the suite in the same change that documents it there.

### End-to-end tests

A second, **manual** layer lives in `tests/e2e/` (a `tests/<name>/main.rs` integration target, so `cargo test --test e2e` addresses it). It spawns the real binary — `env!("CARGO_BIN_EXE_githttp-fs")`, which cargo sets for integration targets only, the reason these cannot live in `src/tests/` — points it at a generated `config.toml` in a `TempDir`, and drives it over the network. No test there imports a single type from the crate: a node is opaque, because the point is to exercise what an operator deploys rather than what a handler returns.

It exists for exactly one boundary the in-crate suite cannot reach, **the process**. Everything in `main()` reports itself by exiting — a config that cannot be read, one that does not parse, one that fails validation, a listener whose port is taken (including the *replication* listener, whose failure must kill a node that is already serving content) — and every one of those ends in `std::process::exit(1)`, observable to a supervisor and to nothing inside the process. The rest needs a **second process lifetime**: that content and history survive a restart, that a stale `.git/index.lock` is cleared at boot, that `checkout_files_autoheal` is the two-step an operator performs (enable for one restart, then turn it back off), and above all that **promotion is a config swap** — promotion being the failover path, where a read-only replica takes over as the write node after the master is lost: flip `role`, drop `master_url`, add `[hooks]`, restart. That one is structurally untestable in-process, because the restart is half of the operation. `deployment.rs` brings up a master process and a replica process on their own scratch disks, converging over the peer protocol exactly as the shipped `config.master.toml` / `config.replica.toml` pair describes; the hook receiver stays *in* the test process, since only the node needs to be out of it.

**They are not in CI, and not in the default `cargo test`.** Every test is `#[ignore]`, so an ordinary `cargo test` — the one the release procedure enforces at step 6 — skips all of them, and running them is always an explicit act. The `cargo e2e` alias in `.cargo/config.toml` is that act, spelled as what it does:

```sh
cargo e2e                        # the whole deployment suite
cargo e2e promotion              # one module
cargo e2e --nocapture            # with node logs on stdout
```

Arguments are appended after `--ignored` and reach the test harness directly — never behind a second `--`, which cargo would pass through as a *name filter* and which therefore silently matches nothing and exits `0`. The alias is a convenience over `cargo test --test e2e -- --ignored`, not a different thing: keeping the `#[ignore]` gate rather than excluding the target from `cargo test` (`test = false` in a `[[test]]` section) is deliberate, because an ignored test is still **compiled** on every `cargo test`, so a refactor that breaks the e2e harness is caught immediately instead of at the next explicit run.

**Cutting a release is the one moment they are not optional.** Step 7 of the [release procedure](#release-procedure) runs them and blocks on a failure, immediately after the in-crate suite has passed — deliberately in that order, since a broken unit test is quicker to read than a broken deployment, and an e2e run over an already-failing build tells you nothing new. Outside of that they are a tool a developer reaches for when exercising a deployment. Two consequences of that choice are worth knowing when adding one: they take **real ports**, claimed from a band *below* the kernel's ephemeral range (`net.ipv4.ip_local_port_range`, so 20000–32767 on a default Linux) rather than by asking for `:0` and letting go — a released port from the ephemeral range is one every `bind(:0)` on the machine is competing for, and the in-crate suite holds one of those per test server, so `cargo test` and `cargo e2e` running together was enough to kill a node with `Address already in use`; and every convergence assertion **polls** rather than reading once, because a replica is eventually consistent by design and a single read would assert on a race. A node that panics prints its whole log, since otherwise the interesting half of an e2e failure dies with the `TempDir`.

## Documentation files

Each documentation file has one audience, and they must not grow into copies of each other:

- **`README.md` — keep it minimal.** It is written for a human landing on the repository, so it covers only what that reader needs: what githttp-fs is, how to install and run it, and the reference list of configuration keys. Design rationale, operational notes, and long-form explanation do not belong there. When a README passage grows past what a newcomer needs, move it into `CONSIDERATIONS.md` and leave a one-line link behind rather than a summary.
- **`CONSIDERATIONS.md`** — design and operational notes for someone already running githttp-fs: how read-only replicas behave, why the two health routes are public, and the files githttp-fs reserves inside a tenant repository.
- **`REPLICATION.md`** — the replication peer protocol specification, for operators and for maintainers of the replication implementation.
- **`CLAUDE.md`** — this file, loaded into every coding session, so it stays small: project layout, the rules that hold everywhere, and the development, test, commit and release conventions. API detail and design rationale do not belong here — they go in the two files below, with at most a one-line rule left behind.
- **`API.md`** — the complete API surface: every route, parameter, request body, response shape and webhook payload. A new route, parameter or event is documented there.
- **`DESIGN.md`** — the annotated configuration reference and the reasoning behind every design decision, written for whoever works on the code.
- **`CHANGELOG.md`** — release notes only, written at release time (see [Changelog](#changelog) below).

Mermaid diagrams are rendered by GitHub, so they must parse there. Never use a semicolon inside a `sequenceDiagram` message or note: mermaid treats it as a statement separator, and the text after it is parsed as its own statement, which fails to render.

## Release procedure

To bump the version to `vX.Y.Z`:

1. Update `version` in `Cargo.toml`
2. Update the version in `README.md`
3. Update the version in `debian/rules`
4. Document every change since the previous tag in `CHANGELOG.md` (see [Changelog](#changelog) below)
5. Run `cargo build` to regenerate `Cargo.lock`
6. Run `cargo test` and **enforce a fully passing suite** — this is one of the two points in the workflow where tests are run automatically, and a failure stops the release: fix it, or do not tag
7. Run `cargo e2e` (the alias for `cargo test --test e2e -- --ignored`) and **enforce a fully passing end-to-end suite** — the same rule, one level up: a failure stops the release. It runs *after* step 6 and only once step 6 is green, because a unit-level failure is far quicker to read than a deployment-level one, and an e2e run over a build that is already broken tells you nothing you did not know
8. Commit all changes with message `vX.Y.Z`
9. Tag the commit with `vX.Y.Z`

## Changelog

`CHANGELOG.md` is the single place where changes are recorded, and it is the source of truth for release notes: the GitHub release created by the build workflow only links to it. **Every release must document all of its changes there, and the changelog is only ever written at release time** — never while a feature or fix is being developed. Changes are documented as part of the version bump (step 4 above), in the same `vX.Y.Z` commit, by reviewing everything that landed since the previous tag:

```sh
git log --oneline $(git describe --tags --abbrev=0)..HEAD
```

Do not add a changelog section for an unreleased version, and do not add entries to it while working on changes: a section only exists once its version number has been decided and the release is being cut. Entries describe what the change means to an API user or operator (a new route, a new parameter, a changed response, a new config key), not the commit messages; housekeeping commits with no user-visible effect (Pawfile bumps, formatting passes, comment-only changes) are not listed. Entries for a version that was released with no notes are filled in from its commits in the same way.

### Format

Newest version first. Each version is a `## vX.Y.Z` heading holding one or more of the four sections below, in this order, each omitted when empty. Entries are `*` bullets written in the past tense, with route paths, parameters, config keys and event names in backticks.

```markdown
Changelog
=========

## v1.12.0

### Breaking Changes

* ⚠️ The `position` field of the read file route is now `null` instead of `-1` when the file is not listed in its parent directory's order index.

### New Features

* Added `include_size` option to file listing route, reporting each file's size in bytes.
* Added `GET /v1/:collection_id/:tenant_id/order/*path` route to read a directory's stored file order.

### Changes

* Hook replays now also re-dispatch one `order.updated` event per directory holding an order index, after all file events.
* Updated all dependencies to latest.

### Bug Fixes

* Fixed a recursive folder move leaving a stale entry in the source directory's order index.

## v1.11.0

### New Features

* Added read-only replication, configured with a new `[replication]` section.
```

The four sections mean:

- **Breaking Changes** — anything an existing API client, hook receiver, or deployment must adapt to: a removed or renamed route, parameter, response field, event, or config key, or a changed default. Each entry starts with `⚠️`. A release with breaking changes may open with a bold **⚠️** paragraph telling operators what to do before upgrading.
- **New Features** — new routes, parameters, events, or config keys.
- **Changes** — behaviour changes that are not breaking, performance and security improvements, dependency updates.
- **Bug Fixes** — fixes to behaviour that did not match its documentation.

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
