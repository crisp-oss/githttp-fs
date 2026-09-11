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
docker pull crispim/githttp-fs:v1.11.0
```

Then, provide a configuration file and run it (replace `/path/to/your/githttp-fs/config.toml` with the path to your configuration file):

```bash
docker run -p 5355:5355 -v /path/to/your/githttp-fs/config.toml:/etc/githttp-fs.cfg crispim/githttp-fs:v1.11.0
```

In the configuration file, ensure that:

* `server.host` is set to `0.0.0.0` (this lets githttp-fs be reached from outside the container)
* `server.port` is set to `5355` (this lets githttp-fs be reached from outside the container)

githttp-fs will be reachable from `http://localhost:5355`.

**Install from packages:**

githttp-fs provides [pre-built packages](https://packagecloud.io/crisp-im/githttp-fs) for Debian-based systems (Debian, Ubuntu, etc.).

**Important: githttp-fs only provides 64 bits packages targeting Debian 11 & 12 for now (codenames: `bullseye` & `bookworm`). You will still be able to use them on other Debian versions, as well as Ubuntu.**

First, add the githttp-fs APT repository (eg. for Debian `bookworm`):

```bash
echo "deb [signed-by=/usr/share/keyrings/crisp-im_githttp-fs.gpg] https://packagecloud.io/crisp-im/githttp-fs/debian/ bookworm main" > /etc/apt/sources.list.d/crisp-im_githttp-fs.list
```

```bash
curl -fsSL https://packagecloud.io/crisp-im/githttp-fs/gpgkey | gpg --dearmor -o /usr/share/keyrings/crisp-im_githttp-fs.gpg
```

```bash
apt-get update
```

Then, install the githttp-fs package:

```bash
apt-get install githttp-fs
```

Then, edit the pre-filled githttp-fs configuration file:

```bash
nano /etc/githttp-fs.toml
```

Finally, restart githttp-fs:

```
service githttp-fs restart
```

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

Use the sample [config.toml](https://github.com/crisp-oss/githttp-fs/blob/master/config.toml) configuration file and adjust it to your own environment. It describes a standalone node; [config.master.toml](https://github.com/crisp-oss/githttp-fs/blob/master/config.master.toml) and [config.replica.toml](https://github.com/crisp-oss/githttp-fs/blob/master/config.replica.toml) show the two sides of a replicated set.

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

**[replication]**

* `role` (type: _string_, allowed: `master`, `replica`, no default) — Whether this node serves replicas or follows a master
* `secret` (type: _string_, allowed: any string, no default) — Guards the replication server, and is sent by a replica to its master. Called `secret` rather than `api_key` because no caller of the product's API ever holds it — it authenticates githttp-fs to githttp-fs
* `host` (type: _string_, allowed: IPv4 / IPv6, default: `server.host`) — Address the replication server binds; set it to `127.0.0.1` or a private interface to narrow reach further
* `port` (type: _string_, allowed: TCP ports, default: `5356`) — Port the replication server binds; must differ from `server.port`
* `node_id` (type: _string_, allowed: up to 64 characters of `A-Z`, `a-z`, `0-9`, `.`, `_`, `-`, `:` and IPv6 brackets, no default) — How this node names itself to its peers: its row in the master's roster and its name in every log line. **Required, and unique per deployment** — a master refuses a second notification stream claiming a node id that is already connected from another process (`409`), since two replicas sharing a row would hide each other from an operator. It is telemetry, not authentication (`secret` is what guards the surface)
* `master_url` (type: _string_, allowed: URL, no default) — Base URL of the master's **replication** server (its `[replication] host`/`port`), not its content API — Base URL of the master to follow; required when `role` is `replica`, and rejected when it is `master`
* `poll_interval_secs` (type: _number_, allowed: seconds, default: `60`) — How often a replica compares its whole repository set against the master's; this is what bounds how stale a replica can get, since change notifications are only a latency hint
* `parallelism` (type: _number_, allowed: any number, default: `4`) — How many repositories a replica pulls concurrently while catching up
* `reconnect_backoff_ms` (type: _number_, allowed: time in milliseconds, default: `1000`) — Base delay before a replica re-dials a dropped notification stream; doubles up to a minute
* `deletion_guard` (type: _boolean_, allowed: `true`, `false`, default: `true`) — The mass-deletion guard: while on, a replica refuses a master listing that would delete more than half of the repositories it holds, keeps every local copy, and reports a `deletion_refused` issue instead. Nothing clears a refused deletion on its own, since a restarted replica still holds what it held; to accept one on purpose, set this to `false` for a single restart, then turn it back on

**[maintenance]**

* `enabled` (type: _boolean_, allowed: `true`, `false`, default: `true`) — Whether to run background repository maintenance (repacks Git objects into a single packfile and expires reflogs, so long-lived repositories stay fast and compact)
* `delay_secs` (type: _number_, allowed: seconds, default: `86400`) — How long after the first write to a repository its maintenance pass should run; the timer re-arms on the next write after each pass, and repositories that receive no writes are never maintained
* `destructive_prune` (type: _boolean_, allowed: `true`, `false`, default: `false`) — Whether the maintenance repack may permanently drop unreachable Git objects (garbage left behind by interrupted writes); commit history and past file versions are never affected either way, but with the default `false` maintenance retains every object and can never destroy data
* `maximum_packs` (type: _number_, allowed: `2` or more, no default) — Opt-in pack-count trigger: when a repository holds at least this many packfiles, its maintenance pass runs immediately on the next write (or replicated pack apply) instead of after `delay_secs`. Meant for replicas, where every replicated delta arrives as one more pack and object lookups slow down with each; a master's writes land as loose objects, so on a master it rarely fires. Unset means the timer alone decides

### Considerations

How read-only replicas behave, why two health routes are public, and the one file githttp-fs reserves inside a tenant repository: see [CONSIDERATIONS.md](CONSIDERATIONS.md).

## :fire: Report A Vulnerability

If you find a vulnerability in githttp-fs, you are more than welcome to report it directly to [@crisp-oss](https://github.com/crisp-oss) by sending an encrypted email to [security@crisp.chat](mailto:security@crisp.chat). Do not report vulnerabilities in public GitHub issues, as they may be exploited by malicious people to target production servers running an unpatched githttp-fs server.

**:warning: You must encrypt your email using [@crisp-oss](https://github.com/crisp-oss) GPG public key available at: [Vulnerability Disclosures](https://docs.crisp.chat/guides/others/security-practices/#vulnerability-disclosures).**
