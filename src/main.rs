// Flavio
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

mod config;
mod error;
mod git;
mod hooks;
mod middleware;
mod routes;
mod state;
mod util;
mod validate;

use axum::{
    middleware as axum_middleware,
    routing::{delete, get, post},
    Router,
};
use std::{env, net::SocketAddr, path::Path};
use tokio::net::TcpListener;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use config::Config;
use state::AppState;

#[tokio::main]
async fn main() {
    // Config must be loaded first so the log level it carries can be passed
    // to init_tracing — the subscriber can only be initialised once.
    let config = load_config();

    init_tracing(config.server.log_level.as_deref());

    let repos_path = config.server.repos_path.clone();

    tracing::info!(
        repos_path = %repos_path.display(),
        "starting flavio"
    );

    // Before accepting traffic, remove any lock files left by a previous crash.
    git::cleanup_all_stale_locks(&repos_path);

    // Ensure the repos root directory exists.
    if !repos_path.exists() {
        tracing::debug!(
            repos_path = %repos_path.display(),
            "repos directory absent, creating"
        );

        std::fs::create_dir_all(&repos_path).unwrap_or_else(|create_err| {
            tracing::error!(
                repos_path = %repos_path.display(),
                err = %create_err,
                "failed to create repos directory"
            );

            std::process::exit(1);
        });
    }

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

fn build_router(app_state: AppState) -> Router {
    let api_routes = Router::new()
        // Tenant management
        .route("/:tenant_id", delete(routes::tenant::delete_tenant))
        // File tree listing
        .route("/:tenant_id/files", get(routes::files::list_files))
        // Individual file operations
        .route(
            "/:tenant_id/files/*path",
            get(routes::files::read_file)
                .put(routes::files::write_file)
                .delete(routes::files::delete_file)
                .post(routes::files::move_file),
        )
        // Commit history
        .route("/:tenant_id/commits", get(routes::commits::list_commits))
        .route("/:tenant_id/commits/:sha", get(routes::commits::get_commit))
        .route(
            "/:tenant_id/commits/:sha/revert",
            post(routes::commits::revert_commit),
        )
        // Require a valid Bearer token on every route.
        .layer(axum_middleware::from_fn_with_state(
            app_state.clone(),
            middleware::require_api_key,
        ))
        .with_state(app_state);

    Router::new().nest("/v1", api_routes)
}

fn load_config() -> Config {
    // Allow overriding the config file path via an environment variable.
    let config_path = env::var("CONFIG_PATH").unwrap_or_else(|_| "config.toml".to_string());

    let raw_content = std::fs::read_to_string(Path::new(&config_path)).unwrap_or_else(|read_err| {
        eprintln!("Cannot read config file '{}': {}", config_path, read_err);

        std::process::exit(1);
    });

    toml::from_str::<Config>(&raw_content).unwrap_or_else(|parse_err| {
        eprintln!("Invalid config file '{}': {}", config_path, parse_err);

        std::process::exit(1);
    })
}

fn init_tracing(log_level: Option<&str>) {
    // RUST_LOG takes priority; config value is the fallback; "info" is the default.
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(log_level.unwrap_or("info")));

    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .init();
}
