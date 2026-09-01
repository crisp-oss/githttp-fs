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
docker pull crispim/githttp-fs:v1.10.1
```

Then, provide a configuration file and run it (replace `/path/to/your/githttp-fs/config.toml` with the path to your configuration file):

```bash
docker run -p 5355:5355 -v /path/to/your/githttp-fs/config.toml:/etc/githttp-fs.cfg crispim/githttp-fs:v1.10.1
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

githttp-fs stores one file of its own inside a tenant repository, holding data Git itself cannot express: **`.order.json`**, the presentation order of the directory it sits in (Git tree entries are name-sorted and carry no metadata slot).

```json
{
  "order": ["intro.md", "getting-started/", "advanced.mdx"]
}
```

* **Entirely opt-in:** no `.order.json` is ever written unless you call `PUT /v1/:collection_id/:tenant_id/order[/*path]` or `POST /v1/:collection_id/:tenant_id/files/*path/reorder`. Never use those routes and no reserved file exists anywhere.
* **A separate resource, not an addressable file:** read and write it through `GET` / `PUT` / `DELETE` on `/order[/*path]`, exchanging a plain JSON array of names. To move a single entry instead of replacing the whole list, `POST /files/*path/reorder` with a numerical `position` shifts that one entry into place, or drops it from the index with `position: -1` (files only, unless you pass `allow_prefix_path: true` to position a folder too).
* **Invisible to every `/files` route** — list, count, read, `HEAD`, batch (where it is `null`) — regardless of `include_hidden_files`. `PUT` and move destinations refuse the path with `400`; move sources and `DELETE` answer `404`.
* **Delivers `order.updated` / `order.deleted` webhooks, never `file.*` ones.** `order.updated` carries the directory's complete resulting order, so downstream it is a replace, not a diff. Both are ordinary `[hooks] events` subscriptions, so a receiver that does not list them gets none.
* **Kept up to date automatically:** deleting or moving a file rewrites the affected index in the same commit. Renames keep their position, cross-directory moves append only to an index that already exists, and an emptied index is removed.
* **Applied on read only if asked:** pass `apply_order_index=true` on the file listing route (default `false`); unlisted entries follow in the ordinary order, or pass `implicit_order_default_index` (e.g. `0` or `-1`) to lift them above the ordered ones instead. Reading a single file always reports its own `position` in its parent's index, `-1` when unlisted.

## :fire: Report A Vulnerability

If you find a vulnerability in githttp-fs, you are more than welcome to report it directly to [@crisp-oss](https://github.com/crisp-oss) by sending an encrypted email to [security@crisp.chat](mailto:security@crisp.chat). Do not report vulnerabilities in public GitHub issues, as they may be exploited by malicious people to target production servers running an unpatched githttp-fs server.

**:warning: You must encrypt your email using [@crisp-oss](https://github.com/crisp-oss) GPG public key available at: [Vulnerability Disclosures](https://docs.crisp.chat/guides/others/security-practices/#vulnerability-disclosures).**
