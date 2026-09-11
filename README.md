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
docker pull crispim/githttp-fs:v1.10.2
```

Then, provide a configuration file and run it (replace `/path/to/your/githttp-fs/config.toml` with the path to your configuration file):

```bash
docker run -p 5355:5355 -v /path/to/your/githttp-fs/config.toml:/etc/githttp-fs.cfg crispim/githttp-fs:v1.10.2
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

**[replication]**

* `role` (type: _string_, allowed: `master`, `replica`, no default) — Whether this node serves replicas or follows a master
* `secret` (type: _string_, allowed: any string, no default) — Guards the replication server, and is sent by a replica to its master. Called `secret` rather than `api_key` because no caller of the product's API ever holds it — it authenticates githttp-fs to githttp-fs
* `host` (type: _string_, allowed: IPv4 / IPv6, default: `server.host`) — Address the replication server binds; set it to `127.0.0.1` or a private interface to narrow reach further
* `port` (type: _string_, allowed: TCP ports, default: `5356`) — Port the replication server binds; must differ from `server.port`
* `node_id` (type: _string_, allowed: up to 64 characters of `A-Z`, `a-z`, `0-9`, `.`, `_`, `-`, `:` and IPv6 brackets, default: `host:port`) — How this node names itself to its peers, so the replication health route shows meaningful names instead of anonymous rows; it is telemetry, not authentication (`secret` is what guards the surface). Override it where the bind address is not how peers see this node — behind NAT, in a container, or when several nodes share a host
* `master_url` (type: _string_, allowed: URL, no default) — Base URL of the master's **replication** server (its `[replication] host`/`port`), not its content API — Base URL of the master to follow; required when `role` is `replica`, and rejected when it is `master`
* `poll_interval_secs` (type: _number_, allowed: seconds, default: `60`) — How often a replica compares its whole repository set against the master's; this is what bounds how stale a replica can get, since change notifications are only a latency hint
* `parallelism` (type: _number_, allowed: any number, default: `4`) — How many repositories a replica pulls concurrently while catching up
* `reconnect_backoff_ms` (type: _number_, allowed: time in milliseconds, default: `1000`) — Base delay before a replica re-dials a dropped notification stream; doubles up to a minute

**[maintenance]**

* `enabled` (type: _boolean_, allowed: `true`, `false`, default: `true`) — Whether to run background repository maintenance (repacks Git objects into a single packfile and expires reflogs, so long-lived repositories stay fast and compact)
* `delay_secs` (type: _number_, allowed: seconds, default: `86400`) — How long after the first write to a repository its maintenance pass should run; the timer re-arms on the next write after each pass, and repositories that receive no writes are never maintained
* `destructive_prune` (type: _boolean_, allowed: `true`, `false`, default: `false`) — Whether the maintenance repack may permanently drop unreachable Git objects (garbage left behind by interrupted writes); commit history and past file versions are never affected either way, but with the default `false` maintenance retains every object and can never destroy data

### Considerations

#### Read-only replicas

Configure a `[replication]` section and githttp-fs nodes form a single-master, many-replica set: **writes stay on the master, while replicas keep serving every read route when the master is down.** Omit the section and nothing changes — no replication routes are mounted, no follower runs. See [REPLICATION.md](REPLICATION.md) for the complete peer-protocol reference, including wire formats, reconciliation, safety guarantees, and failure handling.

Replicas copy Git objects rather than replaying events, which is what makes them exact: every read route is answered from HEAD's tree and the object database, so a replica returns byte-identical results — file listings, seeks, counts, order indexes, commit history, batch reads — including for routes added later.

* **Replicas pull; notifications are only a hint.** A replica converges by comparing its own HEAD shas against the master's repository listing and fetching a packfile for whatever differs. Change notifications carry no content, so a replica that misses them — or that was offline for a week — catches up with exactly the same work. Nothing queues up on the master while a replica is away.
* **Replicas dial out, never in.** The notification channel is a long-lived `GET /_replication/events` opened *by the replica*, so replicas need no public URL, no inbound firewall rule, and no entry in the master's configuration; a replica joins by connecting.
* **Catch-up needs no state and survives interruption.** Each repository's objects are written before its ref moves, so an interrupted catch-up leaves every repository either fully at the old commit or fully at the new one. A replica's own refs are its cursor — there is nothing to checkpoint and nothing to resume.
* **A cold replica refuses reads until it actually holds content** — not merely until it can reach its master, since reaching the master reveals what exists without transferring a single byte, and a node in that state would answer `404` for content that does exist. Any content clears the gate (partial beats empty); a replica that merely restarted with content on disk serves immediately.
* **Writes to a replica answer `423`**, with no `Retry-After`, so a failover-aware client can tell "wrong node" from "no such route" *and* from "come back later" — waiting never makes a replica accept a write, only going to the master does. A replica holding no content at all refuses reads too until its first catch-up completes (an empty repository is not stale, it is wrong), and that refusal *is* transient, so it is a `503` with `Retry-After`; one that merely restarted serves its existing content immediately.
* **A replica pins its master's identity.** Every replicating node keeps `.replication.json` in its `repos_path`, holding a random identity a master generates on first start and states to its replicas; a replica stores the first one it receives and refuses to sync with a master stating any other — loudly, in its logs and in `GET /v1/_health/replication` — until an operator deletes the file and restarts it. This is what stops a replica pointed at the wrong deployment, or following a master rebuilt from an empty disk, from silently deleting its tenants and re-cloning. It is a guard against configuration mistakes on a trusted network, not a cryptographic one: `secret` is what guards the surface.
* **`GET /v1` reports replica status** — whether it is ready or still bootstrapping, whether its stream to the master is up, when it last reconciled, and how many repositories are still behind. A master and a standalone node answer `{ "pong": true }` as before.
* **Replicas can chain**, since they serve the replication surface too — useful for geographic tiering.
* **Two public health routes, answered by every node.** `GET /v1/_health/replication` (no credential) reports this node's role, the master's reachability, and every replica following it — with each field attributable to whoever witnessed it: the master observes `stream_connected`/`packs_delivered`, while replicas report their own `pending_repositories`, since only they can know their lag. A replica serves the roster its master last gave it, stamped `replicas_observed_at`, so an answer given while the master is down is visibly stale rather than quietly wrong. It answers on a standalone node too (`role: "standalone"`), so one probe covers a whole deployment. `GET /v1/_health/status` is its companion: name, version, role, `writable`, and uptime, read from memory with no I/O at all. Both are unauthenticated on purpose — the audience is load balancers, rollout probes and monitoring, none of which should have to hold the content API key — and neither names a tenant or opens a repository. Peers read the same replication picture from `/_replication/health` with the replication key.
* **Replication runs on its own HTTP server and port** (`[replication] host`/`port`, default `5356`), serving `/_replication` and nothing else, behind `replication.secret`. That is deliberate protection against a likely mistake: front the content port with nginx on a shared listener and you have also published a surface that hands out whole repositories and a live change stream, with nothing in the proxy config to say so. Two ports make that mistake unavailable. The secret still guards every route there — the port is defence in depth, not a replacement for auth.
* **The protocol version lives in the payloads, not the URL** — `"protocol": 1` in every JSON body, a `hello` frame opening the event stream, and `X-Replication-Protocol` on the binary packfile response. So the paths never move for a protocol change, and your firewall rules never need revisiting.
* **Hooks stay on the master.** A replica never delivers webhooks (that would duplicate every event), so no hooks fire at all while the master is down; repair downstream mirrors afterwards with `POST /v1/:collection_id/:tenant_id/batch/replay/hook`.
* **Promotion is manual.** Two nodes accepting writes would fork two histories that cannot be merged, so there is no automatic failover for the write role.

#### Public health routes

Every route on the API requires `Authorization: Bearer <api_key>` — with exactly two exceptions, both under `/v1/_health`, both `GET`, and both deliberately public:

* **`GET /v1/_health/status`** — what this process is: `status` (`healthy`, or `bootstrapping` on a cold replica), `name`, `version`, `role` (`master` / `replica` / `standalone`), `writable` (whether a write sent here would be accepted at all), `started_at` and `uptime_secs`. It performs **no I/O whatsoever** — every field comes from the config or from an atomic in memory — so it is safe to poll at any interval, and an anonymous caller cannot make the node do work by asking. It always answers `200`, including while bootstrapping: the status code says the process is alive, and the body says what it can serve.
* **`GET /v1/_health/replication`** — the replication picture described above, identical to what peers read from `/_replication/health` with the replication secret.

They are unauthenticated because their audience is precisely the callers that do not hold the content API key and should not need it: load balancers, rollout probes, uptime monitors, and failover-aware clients asking which node takes writes. Requiring the key there would turn a health probe into a secret-distribution problem. `GET /v1` remains the *authenticated* probe — use that one to verify a key.

Neither route names a tenant, opens a repository, or resolves a path, so what is public is metadata about the process and the node set, never about content. The replication body does describe topology (peer node ids, the master's URL with any credentials stripped, the data-set identity, a repository count); none of it is a credential, but a deployment that treats internal hostnames as sensitive should keep the content port off the public internet.

`_health` is the one collection id reserved by this API, and only for those two exact paths: `/v1/_health/:tenant_id/...` still routes to the ordinary tenant routes.

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
