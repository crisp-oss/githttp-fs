// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! The test suite.
//!
//! It is split the way the codebase is: modules that are pure functions of
//! their input are tested directly ([`validate`], [`seek`], [`order_index`],
//! [`config`]), and everything whose contract is an HTTP contract is tested
//! through a real server on a real socket (see [`harness`]).
//!
//! The behaviour asserted here is the behaviour documented in `CLAUDE.md`.
//! Where a test's expectation looks surprising, the documented rule it comes
//! from is quoted in a comment above it — so a future change that breaks a
//! test can be judged against what the API promised, not just against what
//! the code used to do.

pub mod harness;

mod auth;
mod commits;
mod config;
mod files;
mod fixture;
mod hooks;
mod maintenance;
mod order_index;
mod order_routes;
mod prefix_path;
mod replication;
mod seek;
mod util;
mod validate;
mod working_tree;
