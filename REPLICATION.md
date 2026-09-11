# Replication protocol

This document specifies githttp-fs replication: a single-writer, pull-based protocol that copies Git objects from a master to one or more read-only replicas. It is intended for operators and for maintainers of the replication implementation.

Replication is deliberately separate from webhook delivery. Webhooks project file changes to a downstream system; replication makes another githttp-fs node serve the same Git history and therefore the same read API responses.

## Goals and model

- **Single writer:** only a master accepts content writes. A replica is read-only and returns `423 Locked` for writes.
- **Exact reads:** replicas copy commits, trees, blobs, and refs, rather than rebuilding state from events. Given the same HEAD, every Git-backed read route has the same result on either node, including history, file order, seek, count, and batch-read routes.
- **Pull is authoritative:** a replica periodically compares its local repository set and HEADs to the upstream snapshot, then pulls missing objects. This reconciliation converges even if every live notification is lost.
- **Notifications reduce latency only:** the event stream prompts an immediate reconciliation; it is never a source of truth or a durable queue.
- **One repository is atomic:** objects are imported before its ref is advanced. An interrupted pull leaves the local repository entirely at either its prior HEAD or its new HEAD.
- **Chains are supported:** a caught-up replica exposes the same replication listener and may act as another replica's upstream.

The protocol intentionally has no automatic write failover or history merge. Promoting a node is a manual operational decision: two nodes accepting writes would create divergent histories that this service does not merge.

```mermaid
sequenceDiagram
    participant R as Replica
    participant M as Master
    R->>M: GET /_replication/state
    M-->>R: identity + complete repository snapshot
    R->>M: GET /_replication/{collection}/{tenant}/pack?have=HEAD
    M-->>R: Git pack + announced pinned HEAD
    R->>R: Import objects, fast-forward ref atomically
    R->>M: GET /_replication/events
    M-->>R: hello, updates, deletions, heartbeats
    Note over R,M: Events prompt reconciliation; polling remains authoritative
```

## Topology, listeners, and authentication

When `[replication]` is configured, githttp-fs starts a second HTTP server. Replication routes are mounted **only** on this listener, never below the content API listener.

| Surface | Audience | Auth | Default listener |
|---|---|---|---|
| Content API (`/v1/...`) | applications and operators | `server.api_key` | `server.host:5355` |
| Replication API (`/_replication/...`) | githttp-fs peers | `replication.secret` | `server.host:5356` |

Every replication request must use:

```http
Authorization: Bearer <replication.secret>
X-Replication-Protocol: 1
```

The separate listener prevents a content reverse proxy from accidentally publishing the packfile and long-lived event stream. Bind it to a private interface (for example, `127.0.0.1`) or protect it with network policy as appropriate; the separate port is defense in depth, not a replacement for the secret.

A replica's `master_url` must target the **replication** listener, not the content API port.

## Configuration

```toml
[replication]
role = "master" # or "replica"
secret = "a-peer-only-secret"
# host = "127.0.0.1"        # defaults to server.host
# port = 5356                 # defaults to 5356
node_id = "master-eu"        # required, unique per deployment

# Replica-only:
# master_url = "http://master.internal:5356"
# poll_interval_secs = 60
# parallelism = 4
# reconnect_backoff_ms = 1000
# deletion_guard = true      # refuse listings that would delete > half of what this replica holds
```

`master_url` is required for `role = "replica"` and rejected for a master. `node_id` is required and must be unique across the deployment: a master refuses a notification stream (`409 Conflict`) from a process presenting a node id that another process already holds a stream under. It is observability telemetry, not authentication: it is limited to 64 characters from `[A-Za-z0-9._:-]` plus IPv6 brackets. Invalid peer-supplied IDs are treated as absent.

Each replica process also generates a random instance token at startup and sends it as `X-Replication-Instance` on every request. The same node id with the same token is a replica reconnecting after a drop, and replaces its own connection; the same node id with a different token is a second replica wearing the same name, which is the collision the master refuses. Only the stream is refused — `state` and `pack` stay served, so a misnamed replica keeps converging while its operator is told.

The default polling interval bounds normal eventual-consistency delay. `parallelism` bounds the number of repositories pulled at once. A dropped event stream is re-dialed with exponential backoff, capped at 60 seconds.

## Versioning

The URL prefix is stable and intentionally contains no version: `/_replication`. Versioning travels with each protocol message instead:

- JSON bodies carry `"protocol": 1`.
- The event stream begins with a `hello` frame containing `protocol`.
- A pack response carries `X-Replication-Protocol: 1`.
- Replicas stamp `X-Replication-Protocol: 1` on every request.

A version mismatch is logged rather than treated as a process-fatal error, leaving room for a future implementation to dispatch on the advertised payload version without changing firewall or proxy paths.

## Data-set identity and pairing

A replication identity names the **data set**, not an individual node. It is stored at:

```text
<repos_path>/.replication.json
```

```json
{ "identity": "70617f6e9a0dd1b1f0d103e7c69dbedf5030e2976f13bde604c6ec9f49d9ae75" }
```

The identity is a 64-character lowercase hexadecimal value.

| Node role | Identity behavior |
|---|---|
| Master | Generates and persists an identity on first start. |
| Fresh replica | Pins the identity returned by its first successful state listing. |
| Paired replica | Serves its pinned identity to downstream replicas. |
| Standalone | Has no replication identity file. |

Before every reconciliation, a paired replica compares the upstream identity to its pinned identity. A mismatch stops synchronization: it is reported as an unreachable master and an `identity_mismatch` issue in health output, and logged with both identities, node IDs, and the identity-file path. This prevents a bad `master_url` or a rebuilt, empty master from silently causing a replica to delete its data.

The file is also a canary for the store itself. Every node re-reads it on every rescan of its repository index and compares it to the identity loaded at startup. If `repos_path` was unmounted, swapped, or emptied under the running process, the file is gone or different, and the node then serves its listing as **incomplete** — the one kind of listing a replica never infers deletions from — and raises an `identity_file_missing` or `identity_file_changed` issue. Without that check an empty-but-readable store would scan as a complete, empty listing, which is an instruction to every replica to delete everything.

To intentionally pair a replica with a different data set: stop it, delete `<repos_path>/.replication.json`, and restart it. A present but malformed or invalid identity file is a startup error; it is never silently regenerated.

## Endpoints

All endpoints below are on the replication listener and require the replication secret.

| Method | Path | Purpose | Required for convergence? |
|---|---|---|---|
| `GET` | `/_replication/state` | Complete repository/HEAD snapshot | Yes |
| `GET` | `/_replication/:collection_id/:tenant_id/pack?have=` | Transfer Git objects | Yes |
| `GET` | `/_replication/events` | NDJSON latency hints | No |
| `GET` | `/_replication/health` | Cached topology/status view | No |

`state` and `pack` are the only load-bearing endpoints. A replica converges on its poll interval if `events` is unavailable, and failure to fetch `health` affects observability only.

### State snapshot

```http
GET /_replication/state
```

```json
{
  "protocol": 1,
  "identity": "70617f6e9a0dd1b1f0d103e7c69dbedf5030e2976f13bde604c6ec9f49d9ae75",
  "repositories": [
    {
      "collection_id": "docs",
      "tenant_id": "acme",
      "head_sha": "a3f9c1d"
    }
  ],
  "complete": true
}
```

This is the canonical snapshot used by a replica. It is served from an in-memory repository index maintained by commits and tenant deletions, with periodic rescans to discover out-of-process disk changes. A scan that cannot read a directory or open a repository reports `complete: false` and is retried rather than cached as authoritative.

A replica may pull or update repositories named in an incomplete response, but it **must not infer deletions** from one. Only a complete listing authorizes it to remove a local repository absent upstream. This protects replicas from treating an inaccessible upstream directory or a future paginated response as a deletion.

A cold or unpaired replica returns `503` from `state` until its first catch-up is complete and its identity is pinned. Serving an empty, apparently complete snapshot at that point would cause a chained follower to delete its repositories.

All peer-provided collection IDs, tenant IDs, and SHAs are validated before becoming local paths or URL segments.

### Pack transfer

```http
GET /_replication/:collection_id/:tenant_id/pack?have=<sha>
```

The response is `application/x-git-packfile`, streamed rather than materialized as one buffer. It contains all objects reachable from a HEAD resolved at the start of the request, excluding objects reachable from `have`.

Response headers:

```http
X-Replication-Protocol: 1
X-Replication-Head-Sha: <head used to build this pack>
```

`have` is optional: omit it for a full clone. It accepts hexadecimal only. If it is unknown to the upstream repository, the server ignores it and sends a full pack rather than stranding the replica with an error.

The server does not take a tenant write lock while constructing a pack. History is append-only, so objects reachable from the selected commit remain reachable; maintenance only prunes unreachable objects. A concurrent repack may still create a transient libgit2 error, which the replica retries.

On the client, pack download uses a ten-minute read timeout because the upstream may spend substantial time constructing a large pack before sending its first byte. State and health requests use a shorter 30-second timeout.

### Event stream

```http
GET /_replication/events
Accept: application/x-ndjson
```

The response never completes. It is newline-delimited JSON, with a `hello` frame first:

```json
{"event":"hello","protocol":1,"node_id":"master-eu","identity":"70617f6e9a0dd1b1f0d103e7c69dbedf5030e2976f13bde604c6ec9f49d9ae75"}
{"event":"repository.updated","collection_id":"docs","tenant_id":"acme","head_sha":"a3f9c1d"}
{"event":"repository.deleted","collection_id":"docs","tenant_id":"acme"}
{"event":"heartbeat"}
```

`hello` is sent once per stream session, establishes the protocol version, and allows identity mismatch detection before work is queued. The `identity` can be `null` only when the serving node is an unpaired replica.

Frames never carry content. `repository.updated` and `repository.deleted` are hints to reconcile; heartbeats are sent every 20 seconds while idle to keep intermediaries from closing the connection and to make liveness visible. An update frame queues its repository for a pull directly. A deletion frame is never acted on by itself: it triggers a full reconcile, and the listing that reconcile fetches is what decides, under the mass-deletion guard — so a burst of deletion frames can never do what the guard would refuse a listing for.

Notifications use a broadcast channel. A slow subscriber is disconnected rather than buffered indefinitely; it reconnects and performs a full reconciliation, which safely recovers every missed frame. Replication notifications never use the webhook queue, so a slow replica cannot delay webhook delivery or content writes.

### Health

`GET /_replication/health` returns the same status object as the content API's operator endpoint, `GET /v1/_health/replication`. The former requires `replication.secret`; the latter requires no credential at all — it is one of the two public `/v1/_health/*` routes, so monitoring never needs a key to read it.

Every role—master, replica, and standalone—answers the content API endpoint. Replicas also cache their upstream health response after successful reconciliation instead of proxying on demand, so they still return their last known roster while the upstream is unavailable.

```json
{
  "protocol": 1,
  "status": "healthy",
  "issues": [],
  "node": {
    "node_id": "master-eu",
    "role": "master",
    "identity": "70617f6e9a0dd1b1f0d103e7c69dbedf5030e2976f13bde604c6ec9f49d9ae75",
    "repositories": 26
  },
  "master": {
    "node_id": "master-eu",
    "url": null,
    "reachable": true,
    "last_contact_at": 1789120589,
    "last_error": null
  },
  "replicas": [
    {
      "node_id": "replica-1",
      "stream_connected": true,
      "connected_at": 1789120539,
      "last_contact_at": 1789120589,
      "packs_delivered": 12,
      "repositories": 26,
      "pending_repositories": 0,
      "sync": "synced",
      "reported_at": 1789120589
    }
  ],
  "observed_at": 1789120599,
  "replicas_observed_at": 1789120589
}
```

### Status, and what to alert on

`status` is the one field a monitor needs, and it is present on every role:

| `status` | Meaning | Action |
|---|---|---|
| `healthy` | Nothing is wrong that will not fix itself within a poll interval. | None. |
| `degraded` | Converging, but not well: a replica is `stalled` (three consecutive failed passes), a replica's stream is down and it has been silent for over two minutes, or a cold replica is still bootstrapping. | Warn. Usually a master or network outage in progress. |
| `halted` | An `issue` is open and nothing will close it but a person. | Page. Read `issues`. |

On a replica, `status` follows its own follower state. On a master, it folds in every roster row: a replica reporting `halted` makes the master `halted`, a `stalled` or silent one makes it `degraded`, so one probe against the master covers the whole set. `lagging` is normal operation and never degrades anything.

`issues` lists every condition the node has stopped acting on by itself. Each entry carries a `kind`, the repository it concerns where that applies, and `since` (when it first appeared — an issue re-raised on every pass keeps its original timestamp):

| `kind` | Raised by | Meaning |
|---|---|---|
| `replica_ahead` | replica | The announced head for this repository is an *ancestor* of the local one: this replica holds more history than its upstream. Local copy kept and served; sync of this repository suspended. |
| `history_diverged` | replica | Neither head descends from the other for this repository. Same handling. |
| `identity_mismatch` | replica | The upstream states an identity other than the pinned one. Nothing is pulled. |
| `deletion_refused` | replica | A complete listing would delete more than half of this replica's repositories. Refused; updates continue. |
| `identity_file_missing` | any | This node's own `.replication.json` is gone from `repos_path`. Listing served as incomplete. |
| `identity_file_changed` | any | This node's `.replication.json` holds a different identity than the one loaded at startup. Listing served as incomplete. |
| `node_id_collision` | master, replica | Two processes present the same `node_id`. The master refused the second stream; the replica's own stream is being refused. |

The follower's own verdict is `replica.sync`, sent to the master as `X-Replication-Sync` on every state poll so the roster's `sync` column stays current:

| `sync` | Meaning |
|---|---|
| `synced` | Last pass succeeded; nothing pending, nothing locked. |
| `lagging` | Work is pending, or no pass has completed yet. Normal. |
| `stalled` | Three consecutive passes failed. Heals when whatever is failing stops. |
| `halted` | At least one issue is open. |

`node.role` is `master`, `replica`, or `standalone`. On a master (and standalone node), `master` describes the answering node and is reachable by construction. On a replica, `master.reachable` means its last upstream contact succeeded; it is independent of `stream_connected`, because polling can converge with the stream down.

The master observes `stream_connected`, `connected_at`, `last_contact_at`, and `packs_delivered`. The replica reports its own `repositories` and `pending_repositories`. Disconnected replica rows remain in the roster, preserving their last contact information. `replicas_observed_at` is current on a master and records the cached-upstream observation time on a replica.

A replica's health additionally carries a `replica` object with `state` (`ready` or `bootstrapping`), `sync`, `stream_connected`, `last_reconcile_at`, `last_success_at`, `pending_repositories`, `locked_repositories`, and `consecutive_failures`. The API ping response (`GET /v1`) exposes the short form of it: `state`, `sync`, `stream_connected`, `last_reconcile_at`, and `pending_repositories`.

## Reconciliation algorithm

A replica reconciles at startup, after a stream hint/reconnect, and every `poll_interval_secs`:

1. Fetch `state` and validate its protocol, identity, repository identifiers, and HEAD SHAs.
2. Pin the upstream identity on first successful state response, or reject the response if it differs from the already pinned identity.
3. Build the pending set from upstream repositories whose local HEAD differs or whose local repository is absent — skipping any repository that is locked (see below) unless the upstream announces a different head than the one it was locked against.
4. For each pending repository, relate the announced head to the local one from local objects alone. If the replica already holds it as an ancestor of its HEAD, the replica is ahead: lock the repository without requesting anything. Otherwise request a pack with the local HEAD as `have` (or without it for an absent repository).
5. Import the pack, validate that the announced pack HEAD fast-forwards the local ref, and only then advance the ref. A pack that does not fast-forward is classified as ahead or diverged and the repository is locked; the ref never moves and the local copy is never removed.
6. If the upstream state was `complete`, compute the local repositories absent from it. If they number more than half of what this replica holds (and it holds at least two), refuse the whole deletion phase and raise `deletion_refused`; otherwise delete them. If the listing was incomplete, skip this phase entirely.
7. Refresh the cached upstream health roster after successful reconciliation.

The local ref is the cursor; the protocol stores no separate synchronization checkpoint. Thus restarts and missed events are harmless: the next state comparison recomputes the same work.

### Fast-forward, locking, and the no-wipe rule

An incremental pack is always no larger than a full clone in object-set terms: it is the set reachable from upstream HEAD minus objects reachable from local HEAD. Therefore replicas never need a distance or size heuristic, and never need a re-clone in normal operation.

**A replica never deletes a repository on its own initiative.** When the announced head does not fast-forward the local ref, the replica cannot know whether the upstream or itself holds the history that matters — and the two cases in which it happens point in opposite directions:

- **Ahead** (`replica_ahead`): the announced head is an ancestor of the local one. The replica holds more history than its upstream — a master restored from a backup, a chained upstream that restarted while behind, a failback after a manual promotion. The *upstream* is what is behind. This is detected from local objects before any pack is requested, since the replica already holds the announced commit.
- **Diverged** (`history_diverged`): neither head descends from the other. A tenant deleted and re-created under the same coordinates, or a forked history after two nodes accepted writes.

In both cases the local copy is kept and keeps being served, the repository is **locked** out of replication, an issue is raised (the node becomes `halted`), and one error line names both heads. Nothing else changes: other repositories keep syncing.

A lock is re-examined only when the upstream announces a *different* head than the one it was locked against, so a refused pack is not fetched again on every poll. It lifts by itself the moment a sync of that repository succeeds. There are two ways to get there:

1. **Keep the master's history.** Stop nothing. On the replica, delete the repository directory (`<repos_path>/<collection_id>/<tenant_id>/`); the next reconcile finds it absent and clones it in full. This is the manual form of what the old automatic re-clone did, now with a human deciding it.
2. **Keep the replica's history.** Restore or promote so that the upstream's head moves past the replica's (or replace the upstream's copy with the replica's). The next reconcile sees a new announced head, retries, fast-forwards, and lifts the lock.

The same rule guards the deletion phase: a complete listing that would delete more than half of a replica's repositories is refused outright (`deletion_refused`) rather than applied, because a master genuinely removing most of its tenants in one poll interval is rare and a master whose store was swapped or emptied is not. Updates keep flowing while it is refused, and nothing clears the refusal on its own — a restarted replica still holds what it held, so it would refuse again. If the deletion really is intended, set `replication.deletion_guard = false`, restart the replica once, and turn the guard back on; or, since the guard counts deletions relative to what the replica holds, delete the tenants in smaller batches. A `repository.deleted` frame on the notification stream never deletes anything by itself: it triggers a reconcile, and the listing is what decides, so the guard applies to hinted deletions too.

The replica advances to `X-Replication-Head-Sha`, not merely the SHA from an earlier state or event. The pack is built for the response's pinned HEAD and may already include a newer commit than the one that triggered the request.

## Replica serving behavior

Replicas do not have working trees; they serve reads directly from Git's object database and HEAD. A replica repository is initialized without the local `"chore: initialize"` root commit used for writable repositories, avoiding immediate divergence. Consequently, `git status` in a replica repository can show files as deleted; this does not affect the API.

- Content writes are always rejected with `423 Locked`, before the bootstrap gate. This includes write-shaped `POST` routes, except the read-only batch-read route.
- A cold replica with no repositories on disk returns `503` (with `Retry-After`) for content reads until its initial catch-up completes. Its API ping and replication health remain available so it can explain its state.
- A warm replica restarted with existing data serves immediately: stale content is preferable to an empty false `404`.
- Replicas never enqueue content webhooks. When the master was unavailable, repair a webhook receiver afterward with the content API's hook replay endpoint.
- After each successful apply, a replica publishes a replication notification so downstream replicas can chain from it.

Replicas arm ordinary maintenance after an apply because every sync adds a pack. `maintenance.destructive_prune = true` is safe on replicas: any pruned unreachable object can be fetched again. Set `maintenance.maximum_packs` on a replica: with it, a repository that reaches that many packs runs its pass immediately rather than after `delay_secs`, which is what keeps a busy replicated tenant from carrying hundreds of packs — each one another index libgit2 consults on every object lookup — for a day at a time.

## Failure handling and operator guidance

| Condition | Behavior | Operator action |
|---|---|---|
| Event stream down | Polling still converges; `stream_connected` is false. | Check network/proxy behavior if low latency matters. |
| State request fails | No new reconciliation occurs; existing replica data stays available if warm. | Inspect `master.last_error`, listener reachability, and secret. |
| State says `complete: false` | Updates named in state may apply; deletion inference is skipped. | Repair upstream filesystem/repository access. |
| Identity mismatch | Synchronization halts; master is shown unreachable. | Verify the intended upstream. To intentionally re-pair, stop node, delete `.replication.json`, restart. |
| Pack construction/import transient failure | Replica retries on a later reconciliation. | Check logs and storage health; concurrent repacks can cause transient upstream errors. |
| Replica ahead / history diverged | Local copy kept and served; that repository locked out of sync; `replica_ahead` / `history_diverged` issue; `status: halted`. | Decide which history to keep — see [the no-wipe rule](#fast-forward-locking-and-the-no-wipe-rule). Delete the local directory to take the master's; move the master past the replica to keep the replica's. |
| Deletion refused | Listing would delete more than half the replica's repositories; deletions refused, updates continue; `deletion_refused` issue. | Verify the master's store is intact. If the deletion is intended, set `replication.deletion_guard = false`, restart the replica, then turn the guard back on. |
| Identity file missing/changed | The node serves its listing as incomplete; `identity_file_*` issue; `status: halted`. | Check the mount under `repos_path`. Restore the store, or if the store is deliberately new, restart the node. |
| Node id collision | Master refuses the second stream (`409`); polling still converges; `node_id_collision` issue on both sides once it persists. | Give every replica a unique `replication.node_id`. |
| Cold replica | Content reads return `503` until first catch-up drains. | Probe `GET /v1/_health/status` (reports `status: "bootstrapping"`) and `GET /v1/_health/replication`; do not treat cold `404`s as valid. |

Use `GET /v1/_health/replication` for normal monitoring. It works on every role, needs no credential, and does not expose the peer-only replication secret. `GET /v1/_health/status` is the cheaper companion probe — no I/O — reporting this node's version, role, and whether it accepts writes.

## Implementation constraints

- Repositories are indexed in memory for state responses; a periodic scan repairs out-of-process changes. Incomplete scans are never used to infer deletion.
- Pack export uses local object-database operations (`PackBuilder::insert_walk`); import uses `Odb::packwriter`. The binary uses vendored libgit2 and does not rely on libgit2 HTTP/SSH transports.
- Both sides stream large packs. The receiver stages downloads under `.replication-incoming/`, imports them, and cleans abandoned incoming files at startup.
- The replication listener and content listener are both supervised: either failing terminates the process rather than allowing a node to quietly serve stale data indefinitely.
- Replication and webhook queues are intentionally independent. Replication may drop notification hints under backpressure; webhooks preserve strict per-repository delivery order.
