// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! Traversal order: the sequence a hook replay delivers its paths in.
//!
//! A replay re-emits a repository **as if a user were creating it by hand,
//! one folder at a time** — like peeling an onion. A folder's own files come
//! first, and only once that folder is complete are its sub-folders opened,
//! one after the other, each under the very same rule. So by the time a
//! receiver is told about any file, every file sitting directly in each of
//! that file's ancestor folders has already been delivered, and one folder's
//! whole subtree is finished before its next sibling folder begins.
//!
//! Inside one folder, files and sub-folders alike are taken **hidden
//! (dot-prefixed) first**, then by name, compared bytewise — the comparison
//! the file listing sorts names with.
//!
//! The order is a **pure function of the path strings**, not a tree walk, and
//! that is a requirement rather than a convenience: in the `delete` direction
//! the replayed paths are exactly the ones git does *not* hold, so no walk
//! could ever reach them. A comparator serves both directions, and the
//! caller-supplied and the defaulted `files` list, from one definition — and
//! it replaces two orders that were both accidental: git's depth-first
//! name-sorted walk (which descends into `guides/` before delivering its
//! sibling `intro.md`), and, whenever the caller sent a list, whatever order
//! that list happened to be in.
//!
//! Receivers depend on this sequence, so it is a wire contract: changing it
//! is a breaking change, and `src/tests/traverse.rs` pins it.

use std::cmp::Ordering;

/// Orders two repo-root-relative **file** paths: the last component of each
/// is a file, every component before it a folder.
pub fn compare_files(left: &str, right: &str) -> Ordering {
    compare(left, right, true)
}

/// Orders two repo-root-relative **directory** paths (`""` being the
/// repository root): every component is a folder, so a folder sorts before
/// everything beneath it and the root sorts first of all.
pub fn compare_directories(left: &str, right: &str) -> Ordering {
    compare(left, right, false)
}

/// Sorts file paths into delivery order.
pub fn sort_files(paths: &mut [String]) {
    paths.sort_by(|left, right| compare_files(left, right));
}

/// Sorts directory paths into delivery order.
pub fn sort_directories(directories: &mut [String]) {
    directories.sort_by(|left, right| compare_directories(left, right));
}

/// Walks both paths component by component and decides at the first level
/// where they part ways — which is the folder they last share, and therefore
/// the only folder whose ordering rule is in play.
fn compare(left: &str, right: &str, leaf_is_file: bool) -> Ordering {
    let mut left_components = components(left).peekable();
    let mut right_components = components(right).peekable();

    loop {
        let (left_name, right_name) = match (left_components.next(), right_components.next()) {
            (None, None) => return Ordering::Equal,
            // One path ran out first: it is an ancestor of the other, and a
            // folder comes before what it contains.
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(left_name), Some(right_name)) => (left_name, right_name),
        };

        let left_is_file = leaf_is_file && left_components.peek().is_none();
        let right_is_file = leaf_is_file && right_components.peek().is_none();

        // A folder's own files are delivered before any of its sub-folders is
        // opened. Checked before the names are even looked at, so a file `a`
        // and a folder `a/` — which only a caller's `delete` list can hold
        // together — still fall on the right sides.
        match (left_is_file, right_is_file) {
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            _ => {}
        }

        if left_name != right_name {
            return is_visible(left_name)
                .cmp(&is_visible(right_name))
                .then_with(|| left_name.cmp(right_name));
        }
    }
}

/// Splits a path into its components. The repository root is spelled `""`,
/// which must yield *no* component rather than one empty one.
fn components(path: &str) -> impl Iterator<Item = &str> {
    path.split('/').filter(|component| !component.is_empty())
}

/// `false` sorts before `true`, which is what puts hidden entries first.
fn is_visible(name: &str) -> bool {
    !name.starts_with('.')
}
