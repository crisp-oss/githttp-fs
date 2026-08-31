githttp-fs
==========

[![Test and Build](https://github.com/crisp-oss/githttp-fs/actions/workflows/test.yml/badge.svg)](https://github.com/crisp-oss/githttp-fs/actions/workflows/test.yml) [![Build and Release](https://github.com/crisp-oss/githttp-fs/actions/workflows/build.yml/badge.svg)](https://github.com/crisp-oss/githttp-fs/actions/workflows/build.yml) [![dependency status](https://deps.rs/repo/github/crisp-oss/githttp-fs/status.svg)](https://deps.rs/repo/github/crisp-oss/githttp-fs)

**githttp-fs is a single Rust binary that wraps git repositories and exposes them as a file-system-over-HTTP API. Each tenant gets its own git repository on disk.**

Clients can create, read, update, delete, and move eg. `.md`/`.mdx` files via REST — _which is the initial usecase githttp-fs was written for_ — and optionally pin the presentation order of any directory's entries. Every write produces a Git commit. A configurable webhook fires after each commit so downstream systems (e.g. a read-only SQL database) can update themselves.

_Tested at Rust version: `rustc 1.94.0 (4a4ef493e 2026-03-02)`_

**🇵🇹 Crafted in Lisbon, Portugal.**

## How to use it?

### Installation

**Install from Docker Hub:**

You might find it convenient to run githttp-fs via Docker. You can find the pre-built githttp-fs image on Docker Hub as [crispim/githttp-fs](https://hub.docker.com/r/crispim/githttp-fs/).

First, pull the `crispim/githttp-fs` image:

```bash
docker pull crispim/githttp-fs:v1.8.0
```

Then, provide a configuration file and run it (replace `/path/to/your/githttp-fs/config.toml` with the path to your configuration file):

```bash
docker run -p 5355:5355 -v /path/to/your/githttp-fs/config.toml:/etc/githttp-fs.cfg crispim/githttp-fs:v1.8.0
```

In the configuration file, ensure that:

* `server.host` is set to `0.0.0.0` (this lets githttp-fs be reached from outside the container)
* `server.port` is set to `5355` (this lets githttp-fs be reached from outside the container)

githttp-fs will be reachable from `http://localhost:5355`.

**Install from binary:**

A pre-built binary of githttp-fs is shared in the releases on GitHub. You can simply download the latest binary version from the [releases page](https://github.com/crispim/githttp-fs/releases), and run it on your server.

You will still need to provide the binary with the configuration file, so make sure you have a githttp-fs `config.toml` file ready somewhere.

_The binary provided is statically-linked, which means that it will be able to run on any Linux-based system. Still, it will not work on MacOS or Windows machines._

**Install from Cargo:**

If you prefer managing `githttp-fs` via Rust's Cargo, install it directly via `cargo install`:

```bash
cargo install githttp-fs
```

Ensure that your `$PATH` is properly configured to source the Crates binaries, and then run githttp-fs using the `githttp-fs` command.

**Install from source:**

The last option is to pull the source code from Git and compile githttp-fs via `cargo`:

```bash
cargo build --release
```

You can find the built binaries in the `./target/release` directory.

### Configuration

Use the sample [config.toml](https://github.com/crisp-oss/githttp-fs/blob/master/config.toml) configuration file and adjust it to your own environment.

**Available configuration options are commented below, with allowed values:**

**[server]**

* `host` (type: _string_, allowed: IPv4 / IPv6, default: `0.0.0.0`) — Host the githttp-fs server should listen on
* `port` (type: _string_, allowed: TCP ports, default: `5355`) — Port the githttp-fs server should listen on
* `api_key` (type: _string_, allowed: any string, no default) — API key for the githttp-fs HTTP API
* `repos_path` (type: _string_, allowed: UNIX path, no default) — Path to all Git repositories (all tenants are stored in this path)
* `log_level` (type: _string_, allowed: `debug`, `info`, `warn`, `error`, default: `info`) — Verbosity of logging, set it to `error` in production
* `allowed_extensions` (type: _array[string]_, allowed: file extensions eg. `["md", "mdx"]`, default: none) — Optional whitelist of file extensions accepted for file writes and move destinations; when unset, all extensions are accepted

**[hooks]**

* `url` (type: _string_, allowed: URL, default: no default) — URL of the hook receiver, eg. HTTP URL (if any)
* `events` (type: _array[string]_, allowed: `file.created`, `file.updated`, `file.deleted`, `file.moved`, `order.updated` or `order.deleted`, Default: no default) — List of events to send hooks for (the `order.*` events cover changes to a directory's file order index)
* `retry_attempts` (type: _number_, allowed: any number, Default: no default) — Number of re-delivery attempts to run for a Web Hook that failed delivery
* `retry_backoff_ms` (type: _number_, allowed: time in milliseconds, Default: no default) — How long to back-off between re-delivery attempts

**[hooks.auth]**

* `header` (type: _string_, allowed: any HTTP header name, default: no default) — Authorization header name, as sent to the hook receiver (if any)
* `value` (type: _string_, allowed: any HTTP header value, default: no default) — Authorization header value, as sent to the hook receiver (if any)

**[maintenance]**

* `enabled` (type: _boolean_, allowed: `true`, `false`, default: `true`) — Whether to run background repository maintenance (repacks Git objects into a single packfile and expires reflogs, so long-lived repositories stay fast and compact)
* `delay_secs` (type: _number_, allowed: seconds, default: `86400`) — How long after the first write to a repository its maintenance pass should run; the timer re-arms on the next write after each pass, and repositories that receive no writes are never maintained
* `destructive_prune` (type: _boolean_, allowed: `true`, `false`, default: `false`) — Whether the maintenance repack may permanently drop unreachable Git objects (garbage left behind by interrupted writes); commit history and past file versions are never affected either way, but with the default `false` maintenance retains every object and can never destroy data

### Considerations

#### Reserved files

githttp-fs stores one kind of file of its own inside a tenant repository, used to hold data Git itself cannot express:

**`.order.json`** — the file order index of the directory it sits in.

Git tree entries are name-sorted by definition and carry no metadata slot, so a *presentation* order (the order a sidebar or a table of contents should show entries in) has to be stored as data. githttp-fs stores it per directory, as a `.order.json` holding the leaf names of that directory's entries in the order they should be presented:

```json
{
  "order": ["intro.md", "getting-started/", "advanced.mdx"]
}
```

**This is entirely opt-in, and nothing is written until you ask for it.** No `.order.json` is ever created unless you call `PUT /v1/:collection_id/:tenant_id/order[/*path]`. A repository whose order routes are never used holds no reserved file anywhere, and behaves exactly as it did before the feature existed — the same is true of the hooks, since `order.updated` and `order.deleted` are ordinary `[hooks] events` subscriptions that a receiver has to list to get.

**It is a separate resource, not a file you can address.** Read and write it through `GET` / `PUT` / `DELETE` on `/v1/:collection_id/:tenant_id/order[/*path]`, exchanging a plain JSON array of names — that the server keeps it in a `.order.json` blob is an implementation detail, on the same footing as Git itself. Concretely:

* It is **invisible to every `/files` route** — listing, count, read, `HEAD`, and batch read (where it comes back as `null`) — regardless of `include_hidden_files`. It is never counted as a file, and never matched by a name search.
* The file routes **refuse the path**: `PUT`, and a move destination, answer `400` pointing at `/order`; a move source, and `DELETE`, answer `404`, since to those it is simply not a file.
* A change to it delivers as an **`order.updated` / `order.deleted` webhook**, never as a `file.*` one. `order.updated` carries the directory's complete resulting order, so applying it downstream is a replace rather than a diff.

The dot prefix is the Unix convention for metadata, and a courtesy to humans browsing a repository with `git`; it is not the mechanism that hides the file.

**Its upkeep is automatic.** Deleting or moving a file or folder rewrites the affected directory's index in the very same commit, so a downstream order table can never hold a position for something that is gone. A rename inside one directory keeps the entry's position; a move across directories drops it from the source index and appends it to the destination index only if one already exists; an index left with no entries is removed rather than stored empty. Creating a file adds nothing — an unlisted entry sorts to the tail, so indexes stay sparse by default.

**Reading it back is optional too.** Pass `apply_order_index=true` on the file listing route to have every level of the returned tree ordered by its directory's index; unlisted entries follow in the ordinary order (directories first, then alphabetical). It defaults to `false`, so no existing caller's results change.

## :fire: Report A Vulnerability

If you find a vulnerability in githttp-fs, you are more than welcome to report it directly to [@crisp-oss](https://github.com/crisp-oss) by sending an encrypted email to [security@crisp.chat](mailto:security@crisp.chat). Do not report vulnerabilities in public GitHub issues, as they may be exploited by malicious people to target production servers running an unpatched githttp-fs server.

**:warning: You must encrypt your email using [@crisp-oss](https://github.com/crisp-oss) GPG public key available at: [Vulnerability Disclosures](https://docs.crisp.chat/guides/others/security-practices/#vulnerability-disclosures).**
