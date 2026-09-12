// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! Unit tests for `validate.rs` — the security boundary.
//!
//! Every value tested here reaches a filesystem path or a git lookup, so
//! these tests are about what must be *refused*: traversal, absolute paths,
//! git internals, revspecs, and the two spellings of one identity.

use crate::validate;

#[test]
fn collection_and_tenant_ids_accept_the_safe_alphabet() {
    for id in ["docs", "acme", "a", "A-Z_0-9", &"x".repeat(64)] {
        assert!(validate::collection_id(id).is_ok(), "rejected: {}", id);
        assert!(validate::tenant_id(id).is_ok(), "rejected: {}", id);
    }
}

#[test]
fn collection_and_tenant_ids_reject_anything_that_could_reach_the_filesystem() {
    // Empty, too long, and every metacharacter that means something to a
    // path — these become on-disk directory names verbatim.
    for id in [
        "",
        &"x".repeat(65),
        "..",
        "a/b",
        "a\\b",
        ".hidden",
        "a b",
        "a.b",
        "a:b",
        "a\0b",
        "café",
    ] {
        assert!(validate::collection_id(id).is_err(), "accepted: {:?}", id);
        assert!(validate::tenant_id(id).is_err(), "accepted: {:?}", id);
    }
}

#[test]
fn commit_sha_accepts_hexadecimal_within_bounds() {
    for sha in ["a3f9", "A3F9C1D", &"0".repeat(64)] {
        assert!(validate::commit_sha(sha).is_ok(), "rejected: {}", sha);
    }
}

#[test]
fn commit_sha_rejects_revspecs_and_out_of_range_lengths() {
    // The whole point of the hexadecimal-only rule: git revspecs must never
    // reach a lookup, both to keep git semantics out of the API and to make
    // a history-search denial of service impossible.
    for sha in [
        "HEAD",
        "HEAD~1",
        "master@{1}",
        ":/pattern",
        "abc",
        &"0".repeat(65),
        "",
        "zzzz",
    ] {
        assert!(validate::commit_sha(sha).is_err(), "accepted: {:?}", sha);
    }
}

#[test]
fn file_path_strips_leading_slashes_and_keeps_the_rest_verbatim() {
    assert_eq!(
        validate::file_path("/docs/intro.md").unwrap(),
        "docs/intro.md"
    );
    assert_eq!(
        validate::file_path("docs/intro.md").unwrap(),
        "docs/intro.md"
    );
    assert_eq!(validate::file_path("///a.md").unwrap(), "a.md");
}

#[test]
fn file_path_rejects_traversal_git_internals_and_the_empty_path() {
    for path in [
        "",
        "/",
        "../secrets.md",
        "docs/../../etc/passwd",
        "./docs/intro.md",
        ".git/config",
        "docs/.git/config",
    ] {
        assert!(validate::file_path(path).is_err(), "accepted: {:?}", path);
    }
}

#[test]
fn file_path_keeps_a_trailing_slash_that_file_or_folder_path_strips() {
    // The two validators differ exactly here: a folder is naturally spelled
    // with a trailing slash, and only the routes that accept a folder
    // tolerate one. Both spellings must collapse to one identity.
    assert_eq!(
        validate::file_or_folder_path("/docs/guides/").unwrap(),
        "docs/guides"
    );
    assert_eq!(
        validate::file_or_folder_path("docs/guides").unwrap(),
        "docs/guides"
    );

    assert!(validate::file_or_folder_path("/").is_err());
    assert!(validate::file_or_folder_path("../x").is_err());
}

#[test]
fn folder_path_treats_the_root_as_valid_and_empty() {
    // Unlike file_path, an empty result is *valid* here: it means "the
    // repository root", which is what an omitted prefix_path means.
    assert_eq!(validate::folder_path("").unwrap(), "");
    assert_eq!(validate::folder_path("/").unwrap(), "");
    assert_eq!(validate::folder_path("/docs/").unwrap(), "docs");
    assert_eq!(validate::folder_path("docs/sub").unwrap(), "docs/sub");

    for path in ["../x", "docs/../..", "./docs", ".git", "docs/.git"] {
        assert!(validate::folder_path(path).is_err(), "accepted: {:?}", path);
    }
}

#[test]
fn file_extension_is_unrestricted_without_a_whitelist() {
    assert!(validate::file_extension("anything.exe", None).is_ok());
    assert!(validate::file_extension("no-extension", None).is_ok());
}

#[test]
fn file_extension_compares_case_insensitively_and_tolerates_a_leading_dot() {
    let allowed = vec!["md".to_string(), ".mdx".to_string()];

    assert!(validate::file_extension("a.md", Some(&allowed)).is_ok());
    assert!(validate::file_extension("a.MD", Some(&allowed)).is_ok());
    assert!(validate::file_extension("a.mdx", Some(&allowed)).is_ok());

    assert!(validate::file_extension("a.txt", Some(&allowed)).is_err());
    // An extension-less file can never match a whitelist.
    assert!(validate::file_extension("README", Some(&allowed)).is_err());
}

#[test]
fn node_id_admits_address_shaped_labels_but_stays_bounded() {
    for id in ["master-eu", "10.0.0.1:5356", "[::1]:5356", "node_1.local"] {
        assert!(validate::node_id(id).is_ok(), "rejected: {}", id);
    }

    for id in ["", &"x".repeat(65), "node id", "node/1", "node\n"] {
        assert!(validate::node_id(id).is_err(), "accepted: {:?}", id);
    }
}

#[test]
fn replication_identity_is_exactly_64_lowercase_hex_characters() {
    assert!(validate::replication_identity(&"a1".repeat(32)).is_ok());

    // Upper case is rejected on purpose: the value is compared byte for
    // byte, so two spellings of one identity must not be able to exist.
    assert!(validate::replication_identity(&"A1".repeat(32)).is_err());
    assert!(validate::replication_identity(&"a".repeat(63)).is_err());
    assert!(validate::replication_identity(&"a".repeat(65)).is_err());
    assert!(validate::replication_identity("").is_err());
}
