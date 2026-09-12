// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! End-to-end tests: real processes, real config files, scratch disks.
//!
//! # What this target is for, and what it is not
//!
//! The in-crate suite (`src/tests/`) already drives the real router over real
//! HTTP, including a master/replica pair converging over the peer protocol.
//! It runs *inside the test process*, calling `build_router` directly, which
//! leaves exactly one boundary untested: the **process**. Everything in
//! `main()` — CLI parsing, reading a config file off disk, `init_tracing`,
//! the boot-time repairs, every `std::process::exit(1)` path, and the rule
//! that the process dies when either listener returns — is unreachable from
//! there, and so is every property that needs a *second process lifetime*:
//! that content survives a restart, and that promotion really is the config
//! swap the documentation promises.
//!
//! So these tests spawn `env!("CARGO_BIN_EXE_githttp-fs")` — the binary cargo
//! just built — point it at a generated `config.toml` in a `TempDir`, and
//! talk to it over the network like any other client. A node here is opaque:
//! no test in this file imports a single type from the crate, because the
//! whole point is to exercise what an operator deploys rather than what a
//! handler returns.
//!
//! # They are manual, and deliberately not part of CI
//!
//! Every test is `#[ignore]`, so an ordinary `cargo test` — including the one
//! the release procedure runs and enforces — skips the lot. Run them
//! deliberately:
//!
//! ```sh
//! cargo test --test e2e -- --ignored
//! cargo test --test e2e -- --ignored --nocapture   # with node logs
//! ```
//!
//! They cost seconds each (process spawn, replication convergence) and they
//! take real ports, which is the other reason they are opt-in: they are a
//! tool a developer reaches for when exercising a deployment, not a gate.
//!
//! # Scratch disks
//!
//! Every node gets a `TempDir` holding its store, its config file, and its
//! log, and every one of them is removed when the test ends. Nothing here
//! writes to `dev/repositories`, so a failing e2e run cannot leave state
//! behind for the next one — the deployment is built from nothing each time
//! and, where a test restarts a node, rebuilt over the store the previous
//! lifetime left, which is exactly the thing under test.

mod deployment;
mod node;
mod receiver;

mod lifecycle;
mod promotion;
mod replicated;
