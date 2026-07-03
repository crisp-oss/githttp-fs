// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! HTTP route handlers. These are thin orchestration layers: all actual git
//! work lives in `git.rs`, all delivery logic in `hooks.rs`.
//!
//! Every *write* handler follows the same five-step shape, in this order:
//!
//! 1. **Validate** all user-supplied identifiers and paths (`validate.rs`)
//!    before touching anything.
//! 2. **Acquire the tenant write lock** so writes on one repository are
//!    strictly serialised.
//! 3. **Run the git operation on the blocking pool** (`util::run_blocking`)
//!    since libgit2 is synchronous.
//! 4. **Enqueue the hook job while still holding the lock**, which is what
//!    guarantees hook delivery order equals commit order.
//! 5. **Arm background maintenance** for the repository (no-op when a pass
//!    is already pending).
//!
//! Read handlers skip steps 2, 4, and 5 — they never lock, because they only
//! read immutable git objects reachable from HEAD.

pub mod commits;
pub mod files;
pub mod root;
pub mod tenant;

use serde::Deserialize;

/// The `author` object required in every write request body. githttp-fs has
/// no user accounts of its own — the caller (the upstream application) is
/// trusted to say who is making the change, and both fields go verbatim into
/// the git commit's author/committer signature.
#[derive(Deserialize)]
pub struct AuthorRequest {
    pub name: String,
    pub email: String,
}
