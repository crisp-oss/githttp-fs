// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! Unit tests for `order.rs` — the stored format of a directory's file order
//! index and the structural validation of a caller-supplied order.
//!
//! What exists at a path is checked in `git.rs` against HEAD; everything
//! judged from the values alone lives here.

use crate::order;

#[test]
fn the_index_path_is_the_leaf_name_inside_the_directory_it_orders() {
    assert_eq!(order::order_file_path(""), ".order.json");
    assert_eq!(order::order_file_path("docs"), "docs/.order.json");
    assert_eq!(
        order::order_file_path("docs/guides"),
        "docs/guides/.order.json"
    );
}

#[test]
fn an_index_is_recognised_by_its_leaf_name_at_any_depth() {
    // One check per tree entry is what makes the invisibility rule cheap.
    assert!(order::is_order_file(".order.json"));
    assert!(order::is_order_file("docs/.order.json"));
    assert!(order::is_order_file("a/b/c/.order.json"));

    assert!(!order::is_order_file("order.json"));
    assert!(!order::is_order_file("docs/.order.json.md"));
    assert!(!order::is_order_file("docs/intro.md"));
}

#[test]
fn the_directory_of_an_index_is_its_parent_with_the_root_spelled_empty() {
    assert_eq!(order::directory_of_order_file(".order.json"), Some(""));
    assert_eq!(
        order::directory_of_order_file("docs/.order.json"),
        Some("docs")
    );
    assert_eq!(order::directory_of_order_file("docs/intro.md"), None);
}

#[test]
fn split_parent_puts_a_root_level_path_in_the_empty_directory() {
    assert_eq!(order::split_parent("intro.md"), ("", "intro.md"));
    assert_eq!(order::split_parent("docs/intro.md"), ("docs", "intro.md"));
    assert_eq!(
        order::split_parent("docs/guides/intro.md"),
        ("docs/guides", "intro.md")
    );
}

#[test]
fn two_spellings_of_one_entry_compare_equal() {
    // The trailing slash marks a directory in the stored file, but it is not
    // part of the entry's identity — every comparison goes through this.
    assert_eq!(order::entry_name("guides/"), "guides");
    assert_eq!(order::entry_name("guides"), "guides");
    assert_eq!(order::directory_entry("guides"), "guides/");
}

#[test]
fn hiddenness_is_judged_on_the_name_not_the_slash() {
    assert!(order::is_hidden(".templates/"));
    assert!(order::is_hidden(".gitignore"));

    assert!(!order::is_hidden("docs/"));
    assert!(!order::is_hidden("intro.md"));
}

#[test]
fn the_repository_root_displays_as_a_slash() {
    assert_eq!(order::display_directory(""), "/");
    assert_eq!(order::display_directory("docs"), "docs");
}

#[test]
fn a_stored_index_round_trips() {
    let order = vec![
        "intro.md".to_string(),
        "getting-started/".to_string(),
        "advanced.mdx".to_string(),
    ];

    let serialized = order::serialize(&order);

    // Pretty-printed with a trailing newline so a human inspecting the
    // repository reads a sensible diff.
    assert!(serialized.ends_with('\n'));
    assert_eq!(order::parse(&serialized), Some(order));
}

#[test]
fn a_malformed_index_parses_as_no_index_rather_than_an_error() {
    // Only this server writes the file, so a broken one means a hand-edited
    // commit — and that must never turn every listing into a 500.
    assert_eq!(order::parse("not json"), None);
    assert_eq!(order::parse("[]"), None);
    assert_eq!(order::parse(r#"{"order": "nope"}"#), None);

    // A well-formed but empty document is still a valid document.
    assert_eq!(order::parse(r#"{"order": []}"#), Some(vec![]));
}

#[test]
fn a_valid_order_passes() {
    let order = vec!["intro.md".to_string(), "guides/".to_string()];

    assert!(order::validate_order(&order, false).is_ok());
}

#[test]
fn an_empty_order_is_rejected_because_delete_is_what_it_means() {
    let reason = order::validate_order(&[], false)
        .expect_err("an empty order should be rejected")
        .to_string();

    assert!(reason.contains("delete the index"), "{}", reason);
}

#[test]
fn order_entries_must_be_leaf_names() {
    // Leaf names are what confine an index to its own directory and let a
    // relocated subtree keep its indexes verbatim.
    let error = order::validate_order(&["docs/intro.md".to_string()], false)
        .expect_err("a nested path should be rejected");

    assert!(error.to_string().contains("leaf names"), "{}", error);
}

#[test]
fn order_entries_reject_relative_references_git_and_the_index_itself() {
    for entry in [".", "..", ".git", ".order.json", ""] {
        assert!(
            order::validate_order(&[entry.to_string()], true).is_err(),
            "accepted: {:?}",
            entry
        );
    }
}

#[test]
fn the_index_itself_stays_rejected_even_with_hidden_entries_allowed() {
    // The flag opens hidden *entries*, not the one path that is not an entry.
    let error = order::validate_order(&[".order.json".to_string()], true)
        .expect_err("the index should never order itself");

    assert!(error.to_string().contains("the index itself"), "{}", error);
}

#[test]
fn hidden_entries_are_rejected_unless_asked_for() {
    // Rejected rather than dropped: the caller sent a name and a position,
    // and storing a different order than the one they wrote would be worse
    // than telling them the rule.
    let error = order::validate_order(&[".gitignore".to_string()], false)
        .expect_err("a hidden entry should be rejected by default");

    assert!(
        error.to_string().contains("allow_hidden_files"),
        "{}",
        error
    );

    assert!(order::validate_order(&[".gitignore".to_string()], true).is_ok());
    assert!(order::validate_order(&[".templates/".to_string()], true).is_ok());
}

#[test]
fn duplicate_entries_are_rejected_across_both_spellings() {
    let error = order::validate_order(&["guides".to_string(), "guides/".to_string()], false)
        .expect_err("two spellings of one entry are still a duplicate");

    assert!(error.to_string().contains("unique"), "{}", error);
}
