// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! Binary entry point: wires every subsystem together and runs the server.
//!
//! The startup sequence is strictly ordered, and the order matters:
//!
//! 1. **Parse CLI flags** — only `-c/--config` exists; everything else lives
//!    in the config file so deployments are described by one artifact.
//! 2. **Load + validate config** — before tracing is initialised, because the
//!    config carries the log level and a tracing subscriber can only be
//!    installed once per process. Config errors go to stderr via `eprintln!`.
//! 3. **Initialise tracing** — from here on, all diagnostics use `tracing`.
//! 4. **Clean stale git locks** — a previous process killed mid-operation may
//!    have left `.git/index.lock` files behind; at boot no operation can be
//!    live, so they are all removed unconditionally before traffic arrives.
//! 5. **Build `AppState`** — the shared state (config, hook queues,
//!    maintenance scheduler, per-tenant write locks) cloned into every
//!    request handler.
//! 6. **Build the router, bind, serve** — any failure here is fatal: the
//!    process logs and exits non-zero so a supervisor (systemd, Docker)
//!    restarts it rather than leaving a half-alive server.
//!
//! Module map (see each module's own doc for details):
//!
//! - [`config`] — TOML config types and startup validation
//! - [`state`] — shared application state and per-tenant write locks
//! - [`error`] — the single `AppError` type and its HTTP mapping
//! - [`git`] — every libgit2 operation (the heart of the system)
//! - [`hooks`] — ordered, retried webhook delivery
//! - [`maintenance`] — background loose-object packing
//! - [`middleware`] — Bearer API-key guard
//! - [`order`] — the per-directory file-order index format and its rules
//! - [`replication`] — read-only replica following and change notification
//! - [`routes`] — axum HTTP handlers (thin orchestration over `git`)
//!   (including [`routes::replay`] — webhook replay for downstream repair)
//! - [`seek`] — line-based content windowing shared by file read endpoints
//! - [`util`] — `spawn_blocking` wrapper and constant-time comparison
//! - [`validate`] — sanitisation of all user-supplied identifiers and paths

mod checkout;
mod config;
mod error;
mod git;
mod hooks;
mod maintenance;
mod middleware;
mod order;
mod replication;
mod routes;
mod seek;
mod state;
mod traverse;
mod util;
mod validate;

// The test suite. A module of the binary crate rather than a `tests/`
// directory, so it can reach `build_router` and every internal module
// without exposing them through a library target nobody else needs.
#[cfg(test)]
mod tests;

use axum::{
    middleware as axum_middleware,
    routing::{any, delete, get, on, post, MethodFilter},
    Router,
};
use clap::Parser;
use std::{net::SocketAddr, path::Path};
use tokio::net::TcpListener;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use config::Config;
use state::AppState;

/// Command-line interface. Deliberately minimal: the only flag is the config
/// file path, defaulting to `config.toml` in the working directory, so that
/// all deployment knobs live in one declarative file rather than being
/// scattered across CLI flags and environment variables.
#[derive(Parser)]
#[command(about = "Git-based Content Management System served over HTTP")]
struct Cli {
    #[arg(short = 'c', long = "config", default_value = "config.toml")]
    config: String,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Config must be loaded first so the log level it carries can be passed
    // to init_tracing — the subscriber can only be initialised once.
    let config = load_config(&cli.config);

    init_tracing(config.server.log_level.as_deref());

    let repos_path = config.server.repos_path.clone();

    tracing::info!(
        repos_path = %repos_path.display(),
        "starting githttp-fs"
    );

    // Before accepting traffic, remove any lock files left by a previous crash.
    // At this point no request can be in flight, so every lock found on disk
    // is by definition stale and safe to delete regardless of its age.
    git::GitLocks::cleanup_all_stale_locks(&repos_path);

    // Tenant repositories are created and deleted through staging siblings
    // (see `GitStaging`); one a previous process never finished is abandoned
    // for the same reason a stale lock is.
    git::GitStaging::cleanup_abandoned(&repos_path);

    // Packfile downloads abandoned by a previous process are dead weight for
    // the same reason a stale index lock is: at boot nothing can be in
    // flight, so anything found is finished business.
    replication::cleanup_incoming_packs(&repos_path);

    // Who this node is, as far as replication is concerned: a master reads
    // or generates its data-set identity here, a replica reads the one it
    // pinned. A file that exists but cannot be trusted stops the process —
    // silently regenerating an identity on a master would fork every
    // replica off it.
    let identity = replication::ReplicationIdentity::load(&config).unwrap_or_else(|err| {
        tracing::error!(err = %err, "cannot load replication identity");

        std::process::exit(1);
    });

    let app_state = AppState::new(config.clone(), identity);

    // Heals working trees that do not match HEAD — a store that ran with
    // checkout_files off, or one replicated before replication checked
    // anything out. Spawned, so the listener does not wait on disk work
    // nothing reads; a no-op when this node keeps no files on disk, or when
    // checkout_files_autoheal is off.
    checkout::spawn(app_state.clone());

    // Starts the follower when this node is a replica; a no-op otherwise.
    // Deliberately spawned before the listener binds so a cold replica is
    // already pulling by the time it starts refusing traffic with a reason.
    replication::spawn(app_state.clone());

    let router = build_router(app_state.clone());

    let bind_address = format!("{}:{}", config.server.host, config.server.port);

    let socket_address: SocketAddr = bind_address.parse().unwrap_or_else(|parse_err| {
        tracing::error!(
            address = %bind_address,
            err = %parse_err,
            "invalid bind address"
        );

        std::process::exit(1);
    });

    tracing::debug!(address = %socket_address, "binding tcp listener");

    let listener = TcpListener::bind(socket_address)
        .await
        .unwrap_or_else(|bind_err| {
            tracing::error!(
                address = %socket_address,
                err = %bind_err,
                "failed to bind tcp listener"
            );

            std::process::exit(1);
        });

    // Two listeners, never one.
    //
    // The content API and the replication surface are bound separately so that
    // publishing one cannot publish the other. The failure this guards against
    // is mundane and likely: an operator fronts port 5355 with nginx, forgets
    // that a path prefix on that same port hands out whole repositories and a
    // live change stream, and exposes it. A distinct port cannot be reached
    // through a proxy that was only ever pointed at the content port, so the
    // mistake stops being available to make. It also separates the two
    // workloads — a replica streaming a multi-gigabyte clone is not sharing a
    // listener with user reads.
    //
    // Defence in depth, not instead of it: `replication.secret` still guards
    // every route on the internal server.
    let mut servers = tokio::task::JoinSet::new();

    servers.spawn(async move {
        tracing::info!(address = %socket_address, "api listening");

        let outcome = axum::serve(listener, router).await;

        ("api", outcome)
    });

    if config.replication.is_some() {
        let replication_listener = bind_replication_listener(&config).await;
        let replication_router = build_replication_server(app_state);

        servers.spawn(async move {
            let outcome = axum::serve(replication_listener, replication_router).await;

            ("replication", outcome)
        });
    }

    // Both servers are meant to run for the life of the process, so whichever
    // returns first has failed. Exiting on it rather than carrying on with one
    // listener is what keeps a supervisor honest: a node serving reads but no
    // longer replicating is a node quietly going stale.
    if let Some(finished) = servers.join_next().await {
        match finished {
            Ok((name, Err(serve_err))) => {
                tracing::error!(server = name, err = %serve_err, "server exited with error");
            }
            Ok((name, Ok(()))) => {
                tracing::error!(server = name, "server exited unexpectedly");
            }
            Err(join_err) => {
                tracing::error!(err = %join_err, "server task failed");
            }
        }

        std::process::exit(1);
    }
}

/// Binds the replication server's listener, exiting on failure.
///
/// A separate function rather than a parameterised helper shared with the API
/// listener because the two differ in where their address comes from: the
/// replication server falls back to `server.host` but keeps its own port, so
/// an operator who wants it local-only sets `[replication] host` and nothing
/// else moves.
async fn bind_replication_listener(config: &Config) -> TcpListener {
    let replication = config
        .replication
        .as_ref()
        .expect("replication listener requested without a [replication] section");

    let bind_address = format!("{}:{}", replication.host(&config.server), replication.port);

    let socket_address: SocketAddr = bind_address.parse().unwrap_or_else(|parse_err| {
        tracing::error!(
            address = %bind_address,
            err = %parse_err,
            "invalid replication bind address"
        );

        std::process::exit(1);
    });

    let listener = TcpListener::bind(socket_address)
        .await
        .unwrap_or_else(|bind_err| {
            tracing::error!(
                address = %socket_address,
                err = %bind_err,
                "failed to bind replication tcp listener"
            );

            std::process::exit(1);
        });

    tracing::info!(address = %socket_address, "replication listening");

    listener
}

/// Assembles the full `/v1` route table.
///
/// Every route that touches tenant content goes through the same API-key
/// middleware — a multi-tenant content store keeps its public surface as
/// small as it can. `GET /v1` is the *authenticated* no-op a client uses to
/// verify its key works.
///
/// Exactly two routes sit outside that middleware, both under `/v1/_health`
/// (see [`routes::health`]): they answer questions asked before a credential
/// is held, or when the credential is what is in doubt — a load balancer
/// picking a node, a rollout probe, a dashboard covering a deployment — and
/// they name no tenant and open no repository. The only other unauthenticated
/// route is the bare server root `/`, which does nothing but redirect to
/// `/v1`.
fn build_router(app_state: AppState) -> Router {
    let api_routes = Router::new()
        // API root: authenticated no-op for API-key verification. Nesting
        // strips the `/v1` prefix, so this `/` route answers `GET /v1`
        // (body `{ "pong": true }`). Registered with a bare GET filter
        // rather than `get()`, which would implicitly answer HEAD as well —
        // this endpoint is deliberately GET-only.
        .route("/", on(MethodFilter::GET, routes::root::ping))
        // Tenant management
        .route(
            "/{collection_id}/{tenant_id}",
            delete(routes::tenant::delete_tenant),
        )
        // File tree listing (no trailing path segment — the whole repo,
        // optionally scoped/paged via query parameters).
        .route(
            "/{collection_id}/{tenant_id}/files",
            get(routes::files::list_files),
        )
        // File/directory count statistics. Lives under `/count/files` — a
        // literal segment distinct from `/files`, so it can never collide
        // with the `{*path}` wildcard below.
        .route(
            "/{collection_id}/{tenant_id}/count/files",
            get(routes::files::count_files),
        )
        // Batch file reading. Lives under `/batch/files/read` — a literal
        // segment distinct from `/files`, so it can never collide with the
        // `{*path}` wildcard below.
        .route(
            "/{collection_id}/{tenant_id}/batch/files/read",
            post(routes::files::batch_read_files),
        )
        // Individual file operations. Note that POST here is the *move* and
        // *reorder* operations: axum's `{*path}` wildcard cannot match a fixed
        // `/move` or `/reorder` suffix after the wildcard, so the handler
        // receives the full path (including the suffix) and strips/enforces it
        // itself, dispatching on which one it found.
        .route(
            "/{collection_id}/{tenant_id}/files/{*path}",
            get(routes::files::read_file)
                .head(routes::files::file_exists)
                .put(routes::files::write_file)
                .delete(routes::files::delete_file)
                .post(routes::files::post_file),
        )
        // Webhook replay, for repairing a downstream mirror that drifted out
        // of sync. Lives under literal `/batch/replay/hook` segments —
        // distinct from `/files`, so it can never collide with the `{*path}`
        // wildcard above, and sharing the `/batch` prefix with the batch read
        // since both take a caller-supplied file list in one request. It
        // commits nothing (it only enqueues hook work) but still takes the
        // tenant write lock, since it enqueues and queue order must keep
        // matching commit order.
        .route(
            "/{collection_id}/{tenant_id}/batch/replay/hook",
            post(routes::replay::replay_hook),
        )
        // File-order index. A separate resource from the files it orders, so
        // it is a separate route rather than a flag on `/files` — that is what
        // makes its format impossible to bypass. Two registrations because
        // axum's `{*path}` wildcard needs at least one segment, and the
        // repository root's own order must be addressable too.
        .route(
            "/{collection_id}/{tenant_id}/order",
            get(routes::order::read_order_root)
                .put(routes::order::write_order_root)
                .delete(routes::order::delete_order_root),
        )
        .route(
            "/{collection_id}/{tenant_id}/order/{*path}",
            get(routes::order::read_order)
                .put(routes::order::write_order)
                .delete(routes::order::delete_order),
        )
        // Commit history
        .route(
            "/{collection_id}/{tenant_id}/commits",
            get(routes::commits::list_commits),
        )
        .route(
            "/{collection_id}/{tenant_id}/commits/{sha}",
            get(routes::commits::get_commit),
        )
        .route(
            "/{collection_id}/{tenant_id}/commits/{sha}/revert",
            post(routes::commits::revert_commit),
        )
        // Point-in-time rollback, sibling of the revert route above: same
        // files, same POST verb (it records a new commit rather than removing
        // anything from history), other side of the commit.
        .route(
            "/{collection_id}/{tenant_id}/commits/{sha}/rollback",
            post(routes::commits::rollback_commit),
        )
        // Refuse writes when this node is a replica.
        //
        // Layer order matters and reads backwards: `.layer` wraps what came
        // before it, so the guard applied *last* is the outermost and runs
        // *first*. The API-key check must therefore be applied after this
        // one — an unauthenticated caller must get a plain 401 and learn
        // nothing about whether this node is a replica.
        .layer(axum_middleware::from_fn_with_state(
            app_state.clone(),
            middleware::enforce_replica_read_only,
        ))
        // Require a valid Bearer token on every route. Outermost, so it runs
        // before anything else can answer.
        .layer(axum_middleware::from_fn_with_state(
            app_state.clone(),
            middleware::require_api_key,
        ))
        .with_state(app_state.clone());

    // The public health surface. Nested onto the API router *after* both
    // `.layer` calls above, which is precisely what leaves it unauthenticated:
    // a layer wraps the routes registered before it and nothing added
    // afterwards, so these two never see the API-key guard or the replica
    // read-only guard. That ordering is load-bearing — moving this above the
    // layers would silently put a key back in front of every monitor — which
    // is why it is a separate router built here rather than two more `.route`
    // calls in the table above.
    //
    // It is also why the read-only guard no longer has to make an exception
    // for the status routes: a replica refusing everything else can still
    // explain itself, because the explanation never reaches the guard.
    let health_routes = Router::new()
        .route("/status", get(routes::health::health_status))
        .route("/replication", get(routes::health::health_replication))
        .with_state(app_state);

    let versioned_routes = api_routes.nest("/_health", health_routes);

    Router::new()
        // Bare server root: any method, no auth — just point the caller at
        // the versioned API prefix with a 308 (method-preserving) redirect.
        .route("/", any(routes::root::redirect_to_api_root))
        .nest("/v1", versioned_routes)
}

/// Assembles the internal replication server: the routes under
/// [`replication::URL_PREFIX`] (`/_replication`), on their own listener.
///
/// **Why a separate server rather than a prefix on the content one.** Four
/// things separate this surface from the content API, and a shared listener
/// expresses none of them well:
///
/// 1. **It must not be publishable by accident.** This is the decisive one.
///    An operator who fronts the content port with nginx has, on a shared
///    listener, also fronted a surface that hands out whole repositories and
///    a live change stream — and nothing about the proxy config says so. On
///    its own port that mistake is not available to make: a proxy pointed at
///    the content port simply cannot reach it.
/// 2. **Its own credential.** Every content route takes `server.api_key`;
///    these take `replication.secret`. On a shared listener that is an
///    exception to a rule stated plainly everywhere else, and exceptions to
///    auth rules are where mistakes live. On its own server it is just the
///    rule for that server.
/// 3. **Its own network policy.** Binding it to `127.0.0.1` or a private
///    interface (`[replication] host`) is a one-line decision, where carving
///    path exceptions out of a published listener is a config review.
/// 4. **No namespace to steal, and no version to agree on.** Nothing here
///    shares a path space with tenants, so no collection id has to be
///    reserved; and the protocol version travels in the payloads
///    ([`replication::PROTOCOL_VERSION`]) rather than in the URL, so the
///    paths — and every firewall rule naming them — stay stable across
///    protocol changes.
///
/// Every route is a `GET`: replication only ever *reads* from the node serving
/// it. The replica read-only guard is layered on the content router alone, so
/// it never sees these routes at all rather than passing them by accident.
fn build_replication_server(app_state: AppState) -> Router {
    let replication_routes = Router::new()
        .route("/state", get(routes::replication::replication_state))
        .route("/health", get(routes::replication::replication_health))
        .route("/events", get(routes::replication::replication_events))
        .route(
            "/{collection_id}/{tenant_id}/pack",
            get(routes::replication::replication_pack),
        )
        .layer(axum_middleware::from_fn_with_state(
            app_state.clone(),
            middleware::require_replication_key,
        ))
        .with_state(app_state);

    Router::new().nest(replication::URL_PREFIX, replication_routes)
}

/// Reads, parses, and validates the TOML config, exiting the process on any
/// failure. Uses `eprintln!` rather than `tracing` because this runs before
/// the tracing subscriber exists (the config itself carries the log level).
/// Validation errors are all collected and printed together so an operator
/// can fix every mistake in one edit instead of playing whack-a-mole.
fn load_config(config_path: &str) -> Config {
    let raw_content = std::fs::read_to_string(Path::new(config_path)).unwrap_or_else(|read_err| {
        eprintln!("Cannot read config file '{}': {}", config_path, read_err);

        std::process::exit(1);
    });

    let config = toml::from_str::<Config>(&raw_content).unwrap_or_else(|parse_err| {
        eprintln!("Invalid config file '{}': {}", config_path, parse_err);

        std::process::exit(1);
    });

    if let Err(validation_errors) = config.validate() {
        for error in &validation_errors {
            eprintln!("Config error: {}", error);
        }

        std::process::exit(1);
    }

    config
}

/// Installs the global tracing subscriber. Can only ever be called once per
/// process — which is why config loading must happen first.
fn init_tracing(log_level: Option<&str>) {
    // Verbosity priority: RUST_LOG env var → config `log_level` → "info".
    // The env var wins so an operator can crank up logging on a running
    // deployment without editing the config file.
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(log_level.unwrap_or("info")));

    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .init();
}
