// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! Tests for `GitStaging`: a tenant repository appears at its final path
//! only once it is complete, and disappears from it in one step.
//!
//! Tested directly rather than through the routes because the property is
//! about what a concurrent reader could observe *during* the operation, which
//! only the build closure can look at deterministically.

use std::path::Path;

use crate::error::AppError;
use crate::git::GitStaging;

fn entries(directory: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(directory)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();

    names.sort();

    names
}

#[test]
fn a_repository_under_construction_is_invisible_at_its_final_path() {
    let store = tempfile::tempdir().unwrap();
    let repo_path = store.path().join("docs").join("acme");

    GitStaging::create(&repo_path, |staging| {
        // What a reader resolving the tenant would find right now: nothing.
        assert!(!repo_path.exists());

        // And the staging directory is a sibling no identifier can name.
        assert_eq!(staging.parent(), repo_path.parent());
        assert!(staging
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with('.'));

        git2::Repository::init(staging)?;

        Ok(())
    })
    .expect("creation failed");

    assert!(repo_path.join(".git").is_dir());
    assert_eq!(entries(&store.path().join("docs")), vec!["acme"]);
}

#[test]
fn a_failed_build_leaves_nothing_behind() {
    let store = tempfile::tempdir().unwrap();
    let repo_path = store.path().join("docs").join("acme");

    let outcome: Result<(), AppError> = GitStaging::create(&repo_path, |staging| {
        git2::Repository::init(staging)?;

        Err(AppError::InvalidOperation {
            reason: "the pack was incomplete".to_string(),
        })
    });

    assert!(outcome.is_err());
    assert!(!repo_path.exists());
    assert!(entries(&store.path().join("docs")).is_empty());
}

#[test]
fn removing_a_repository_leaves_nothing_behind() {
    let store = tempfile::tempdir().unwrap();
    let repo_path = store.path().join("docs").join("acme");

    git2::Repository::init(&repo_path).unwrap();
    std::fs::write(repo_path.join("a.md"), "x").unwrap();

    GitStaging::remove(&repo_path).expect("removal failed");

    assert!(!repo_path.exists());
    assert!(entries(&store.path().join("docs")).is_empty());
}

#[test]
fn the_startup_sweep_removes_only_staging_directories() {
    let store = tempfile::tempdir().unwrap();
    let collection = store.path().join("docs");

    // Abandoned by a previous process: removed.
    for name in [".acme.creating-3", ".acme.deleting-0"] {
        std::fs::create_dir_all(collection.join(name).join(".git")).unwrap();
    }

    // Anything else, however close its name: kept.
    for name in [
        "acme",
        ".notes",
        ".acme.creating-",
        ".acme.creating-x",
        ".a.b.deleting-1",
    ] {
        std::fs::create_dir_all(collection.join(name)).unwrap();
    }

    GitStaging::cleanup_abandoned(store.path());

    assert_eq!(
        entries(&collection),
        vec![
            ".a.b.deleting-1",
            ".acme.creating-",
            ".acme.creating-x",
            ".notes",
            "acme"
        ]
    );
}
