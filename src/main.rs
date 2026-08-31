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
//! - [`routes`] — axum HTTP handlers (thin orchestration over `git`)
//!   (including [`routes::replay`] — webhook replay for downstream repair)
//! - [`seek`] — line-based content windowing shared by file read endpoints
//! - [`util`] — `spawn_blocking` wrapper and constant-time comparison
//! - [`validate`] — sanitisation of all user-supplied identifiers and paths

mod config;
mod error;
mod git;
mod hooks;
mod maintenance;
mod middleware;
mod order;
mod routes;
mod seek;
mod state;
mod util;
mod validate;

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

    let app_state = AppState::new(config.clone());
    let router = build_router(app_state);

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

    tracing::info!(address = %socket_address, "listening");

    axum::serve(listener, router)
        .await
        .unwrap_or_else(|serve_err| {
            tracing::error!(err = %serve_err, "server exited with error");

            std::process::exit(1);
        });
}

/// Assembles the full `/v1` route table.
///
/// Every API route goes through the same API-key middleware — there are no
/// unauthenticated API endpoints (not even a health check), which keeps the
/// public surface of a multi-tenant content store as small as possible.
/// The closest thing to a health check is `GET /v1`, an authenticated
/// no-op that lets a client verify its API key works. The only route
/// outside the middleware is the bare server root `/`, which does nothing
/// but redirect to `/v1`.
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
        // Require a valid Bearer token on every route.
        .layer(axum_middleware::from_fn_with_state(
            app_state.clone(),
            middleware::require_api_key,
        ))
        .with_state(app_state);

    Router::new()
        // Bare server root: any method, no auth — just point the caller at
        // the versioned API prefix with a 308 (method-preserving) redirect.
        .route("/", any(routes::root::redirect_to_api_root))
        .nest("/v1", api_routes)
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
