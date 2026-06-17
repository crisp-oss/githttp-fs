// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

pub mod commits;
pub mod files;
pub mod tenant;

use serde::Deserialize;

#[derive(Deserialize)]
pub struct AuthorRequest {
    pub name: String,
    pub email: String,
}
