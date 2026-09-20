// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! Unit tests for `traverse.rs` — the order a hook replay delivers paths in.
//!
//! **This order is a wire contract.** Receivers rebuild a tree from replayed
//! events and rely on being told about a folder's files before anything
//! beneath it, as if a user were creating the repository by hand, level by
//! level — like peeling an onion. A test failing here therefore means
//! dependent implementations break: the fix is to restore the order, not to
//! update the expectation. The rules, all of which are pinned below:
//!
//! 1. A folder's own files are delivered before any of its sub-folders is
//!    opened.
//! 2. A sub-folder's whole subtree is delivered before its next sibling
//!    folder begins.
//! 3. Inside one folder, hidden (dot-prefixed) entries come first — files
//!    among files, folders among folders — then names compare bytewise.
//! 4. The result depends on the *set* of paths alone, never on the order
//!    they were supplied in.

use std::cmp::Ordering;

use crate::traverse::{compare_directories, compare_files, sort_directories, sort_files};

/// Sorts `paths` as the replay route does.
fn sorted_files(paths: &[&str]) -> Vec<String> {
    let mut paths: Vec<String> = paths.iter().map(|path| path.to_string()).collect();

    sort_files(&mut paths);

    paths
}

fn sorted_directories(directories: &[&str]) -> Vec<String> {
    let mut directories: Vec<String> = directories
        .iter()
        .map(|directory| directory.to_string())
        .collect();

    sort_directories(&mut directories);

    directories
}

/// The tree every ordering rule shows up in at once, already in the one
/// correct delivery order. Tests feed it in other orders and expect this back.
const ONION: &[&str] = &[
    // The root's own files: hidden first, then bytewise ('-' < 'R' < 'z').
    ".meta.md",
    "-notes.md",
    "README.md",
    "zed.md",
    // The root is complete, so its folders open — hidden folders first.
    ".templates/.partial.md",
    ".templates/page.md",
    ".templates/blocks/hero.md",
    // `docs/`: its own files, then its sub-folders, each finished in turn.
    "docs/.draft.md",
    "docs/intro.md",
    "docs/guides/setup.md",
    "docs/guides/advanced/tuning.md",
    "docs/reference/api.md",
    // A sibling folder starts only once all of `docs/` is done.
    "legal/terms.md",
];

#[test]
fn the_documented_tree_is_delivered_in_the_documented_order() {
    // Git's own walk order for the same tree — depth-first, name-sorted —
    // which is what a replay used to deliver and must not deliver again:
    // `docs/guides/…` ahead of `docs/intro.md`, `zed.md` after every folder.
    let git_walk_order = [
        "-notes.md",
        ".meta.md",
        ".templates/.partial.md",
        ".templates/blocks/hero.md",
        ".templates/page.md",
        "README.md",
        "docs/.draft.md",
        "docs/guides/advanced/tuning.md",
        "docs/guides/setup.md",
        "docs/intro.md",
        "docs/reference/api.md",
        "legal/terms.md",
        "zed.md",
    ];

    assert_eq!(sorted_files(&git_walk_order), ONION);
}

#[test]
fn the_order_does_not_depend_on_the_order_paths_were_supplied_in() {
    // A caller's `files` list arrives in whatever order its mirror produced.
    let mut reversed: Vec<&str> = ONION.to_vec();

    reversed.reverse();

    assert_eq!(sorted_files(&reversed), ONION);

    // Every rotation too, so no starting arrangement is privileged.
    for offset in 0..ONION.len() {
        let mut rotated: Vec<&str> = ONION.to_vec();

        rotated.rotate_left(offset);

        assert_eq!(sorted_files(&rotated), ONION, "rotated by {}", offset);
    }
}

#[test]
fn every_file_follows_the_files_of_all_its_ancestor_folders() {
    // The property receivers actually rely on, checked independently of the
    // expected list above: when a file arrives, no file sitting directly in
    // one of its ancestor folders is still to come.
    let delivered = sorted_files(ONION);

    for (index, path) in delivered.iter().enumerate() {
        for later in &delivered[index + 1..] {
            let folder = path.rsplit_once('/').map_or("", |(folder, _)| folder);
            let later_folder = later.rsplit_once('/').map_or("", |(folder, _)| folder);

            let later_sits_in_an_ancestor = later_folder != folder
                && (later_folder.is_empty() || folder.starts_with(&format!("{}/", later_folder)));

            assert!(
                !later_sits_in_an_ancestor,
                "{} was delivered before {}, a file of one of its ancestor folders",
                path, later
            );
        }
    }
}

#[test]
fn a_subtree_is_contiguous() {
    // Rule 2: once a folder is opened, nothing outside it is delivered until
    // it is finished.
    let delivered = sorted_files(ONION);

    for folder in ["docs/", "docs/guides/", ".templates/"] {
        let positions: Vec<usize> = delivered
            .iter()
            .enumerate()
            .filter(|(_, path)| path.starts_with(folder))
            .map(|(index, _)| index)
            .collect();

        let span = positions.last().unwrap() - positions.first().unwrap() + 1;

        assert_eq!(span, positions.len(), "{} is interleaved", folder);
    }
}

#[test]
fn a_file_precedes_a_folder_whatever_their_names() {
    // Rule 1 is decided before names are looked at.
    assert_eq!(compare_files("zzz.md", "aaa/file.md"), Ordering::Less);
    assert_eq!(compare_files("aaa/file.md", "zzz.md"), Ordering::Greater);

    // Including against a *hidden* folder: hidden-first ranks entries of the
    // same kind, it does not lift a folder above its parent's files.
    assert_eq!(
        compare_files("visible.md", ".hidden/file.md"),
        Ordering::Less
    );

    // And at depth, not just at the root.
    assert_eq!(
        compare_files("a/b/zzz.md", "a/b/aaa/file.md"),
        Ordering::Less
    );
}

#[test]
fn hidden_entries_lead_among_their_own_kind() {
    // '-' (0x2D) sorts below '.' (0x2E) bytewise, which is why hidden-first
    // has to be an explicit rule rather than a side effect of name order.
    assert_eq!(compare_files(".hidden.md", "-dash.md"), Ordering::Less);
    assert_eq!(compare_files(".hidden.md", "!bang.md"), Ordering::Less);
    assert_eq!(compare_files(".hidden/x.md", "-dash/x.md"), Ordering::Less);

    // Two hidden entries fall back to their names.
    assert_eq!(compare_files(".a.md", ".b.md"), Ordering::Less);
}

#[test]
fn names_compare_bytewise_like_the_listing() {
    // Uppercase before lowercase, digits before both.
    assert_eq!(
        sorted_files(&["b.md", "B.md", "a.md", "1.md", "A.md"]),
        ["1.md", "A.md", "B.md", "a.md", "b.md"]
    );
}

#[test]
fn a_file_and_a_folder_sharing_a_name_still_order_file_first() {
    // Impossible inside git, but a `delete` replay orders whatever the
    // mirror says it holds, and the order must stay total and deterministic.
    assert_eq!(compare_files("a", "a/b.md"), Ordering::Less);
    assert_eq!(compare_files("a/b.md", "a"), Ordering::Greater);
    assert_eq!(sorted_files(&["a/b.md", "a"]), ["a", "a/b.md"]);
}

#[test]
fn identical_paths_are_equal() {
    assert_eq!(compare_files("docs/a.md", "docs/a.md"), Ordering::Equal);
    assert_eq!(compare_directories("docs", "docs"), Ordering::Equal);
    assert_eq!(compare_directories("", ""), Ordering::Equal);
}

#[test]
fn the_comparison_is_antisymmetric_and_transitive_over_the_whole_tree() {
    // `sort_by` is only meaningful over a total order; an inconsistent
    // comparator would make the delivered sequence depend on input order.
    for (left_index, left) in ONION.iter().enumerate() {
        for (right_index, right) in ONION.iter().enumerate() {
            assert_eq!(
                compare_files(left, right),
                left_index.cmp(&right_index),
                "{} vs {}",
                left,
                right
            );
        }
    }
}

#[test]
fn directories_open_top_down_with_the_root_first() {
    // The order phase: one `order.updated` per directory, parents before
    // children, hidden folders leading, each subtree finished in turn.
    assert_eq!(
        sorted_directories(&[
            "legal",
            "docs/reference",
            "docs/guides/advanced",
            "docs",
            ".templates/blocks",
            "",
            "docs/guides",
            ".templates",
        ]),
        [
            "",
            ".templates",
            ".templates/blocks",
            "docs",
            "docs/guides",
            "docs/guides/advanced",
            "docs/reference",
            "legal",
        ]
    );
}

#[test]
fn a_directory_precedes_everything_beneath_it_even_a_hidden_child() {
    assert_eq!(compare_directories("docs", "docs/.hidden"), Ordering::Less);
    assert_eq!(compare_directories("", ".hidden"), Ordering::Less);
}
