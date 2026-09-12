// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! Unit tests for `seek.rs` — the line-window scan and its two wire formats.
//!
//! Two properties are asserted throughout, because both are documented
//! promises rather than incidental behaviour:
//!
//! - The window is returned **byte-for-byte**: CRLF endings and the presence
//!   or absence of a final newline survive.
//! - The `to` search starts on the line *after* the window's first line, so
//!   the same prefix can bound both ends (`--- … ---` selects a whole
//!   front-matter block, both markers included).

use std::io::Cursor;

use crate::seek::{SeekBody, SeekFilter, SeekOptions};

/// Runs a filter over `content`, as the git layer does.
#[track_caller]
fn apply(filter: &SeekFilter, content: &str) -> String {
    filter
        .apply_reader(Cursor::new(content.as_bytes().to_vec()), "test.md")
        .expect("seek scan failed")
}

/// Builds a query-spelled filter, panicking on a rejection the test did not
/// expect.
#[track_caller]
fn query(from: Option<&str>, to: Option<&str>, maximum: Option<usize>) -> SeekFilter {
    SeekOptions {
        seek_from_line_starts_with: from.map(str::to_string),
        seek_to_line_starts_with: to.map(str::to_string),
        seek_lines_maximum: maximum,
    }
    .parse()
    .expect("seek options rejected")
}

const FRONT_MATTER: &str = "---\ntitle: Hello\n---\n# Heading\n\nBody text\n";

#[test]
fn no_filter_is_a_noop() {
    let filter = query(None, None, None);

    assert!(filter.is_noop());
    assert_eq!(apply(&filter, FRONT_MATTER), FRONT_MATTER);
}

#[test]
fn from_prefix_opens_the_window_at_the_matching_line_inclusive() {
    let filter = query(Some(r##"["# "]"##), None, None);

    assert_eq!(apply(&filter, FRONT_MATTER), "# Heading\n\nBody text\n");
}

#[test]
fn from_prefix_matching_nothing_yields_an_empty_window() {
    // Documented: when no line matches, `content` is empty — still a 200.
    let filter = query(Some(r#"["@@@"]"#), None, None);

    assert_eq!(apply(&filter, FRONT_MATTER), "");
}

#[test]
fn to_prefix_closes_the_window_at_the_matching_line_inclusive() {
    let filter = query(None, Some(r##"["# "]"##), None);

    assert_eq!(
        apply(&filter, FRONT_MATTER),
        "---\ntitle: Hello\n---\n# Heading\n"
    );
}

#[test]
fn the_same_prefix_can_bound_both_ends() {
    // The `to` search begins on the line *after* the window's first line,
    // which is exactly what makes this work: from `---` to `---` selects the
    // whole front-matter block with both markers.
    let filter = query(Some(r#"["---"]"#), Some(r#"["---"]"#), None);

    assert_eq!(apply(&filter, FRONT_MATTER), "---\ntitle: Hello\n---\n");
}

#[test]
fn to_prefix_matching_nothing_runs_to_the_end_of_the_file() {
    let filter = query(Some(r##"["# "]"##), Some(r#"["@@@"]"#), None);

    assert_eq!(apply(&filter, FRONT_MATTER), "# Heading\n\nBody text\n");
}

#[test]
fn lines_maximum_caps_the_window_from_its_first_line() {
    let filter = query(None, None, Some(2));

    assert_eq!(apply(&filter, FRONT_MATTER), "---\ntitle: Hello\n");

    // Counted from the `from` match, not from line 0.
    let filter = query(Some(r##"["# "]"##), None, Some(1));

    assert_eq!(apply(&filter, FRONT_MATTER), "# Heading\n");
}

#[test]
fn filters_resolve_from_then_to_then_maximum() {
    // The maximum narrows a window the `to` marker already closed.
    let filter = query(Some(r#"["---"]"#), Some(r#"["---"]"#), Some(2));

    assert_eq!(apply(&filter, FRONT_MATTER), "---\ntitle: Hello\n");
}

#[test]
fn the_first_matching_prefix_in_order_wins() {
    // Documented tie-break: on a line matching several prefixes, the first
    // in the given order wins — which is what the meta operator resolves to.
    let content = "+++\ntitle: Hello\n+++\nBody\n";

    let filter = query(
        Some(r#"["---", "+++"]"#),
        Some(r#"["$seek_from_line_starts_with"]"#),
        None,
    );

    assert_eq!(apply(&filter, content), "+++\ntitle: Hello\n+++\n");
}

#[test]
fn the_meta_operator_resolves_to_whichever_from_prefix_matched() {
    // Same seek, two files using different front-matter markers: each stops
    // on its own marker.
    let filter = query(
        Some(r#"["---", "+++"]"#),
        Some("$seek_from_line_starts_with"),
        None,
    );

    assert_eq!(apply(&filter, "---\na: 1\n---\nbody\n"), "---\na: 1\n---\n");
    assert_eq!(
        apply(&filter, "+++\na = 1\n+++\nbody\n"),
        "+++\na = 1\n+++\n"
    );
}

#[test]
fn the_window_preserves_crlf_and_a_missing_final_newline() {
    let filter = query(Some(r#"["b"]"#), None, None);

    assert_eq!(apply(&filter, "a\r\nb\r\nc"), "b\r\nc");

    let filter = query(None, None, Some(1));

    assert_eq!(apply(&filter, "a\r\nb\r\n"), "a\r\n");
}

#[test]
fn a_window_is_not_required_to_decode_bytes_outside_it() {
    // Prefix matching is byte-level, and only the selected window must be
    // valid UTF-8 — that is what lets the scan stop early without decoding
    // the rest of the object.
    let mut content = b"# Heading\n".to_vec();

    content.extend_from_slice(&[0xff, 0xfe, b'\n']);

    let filter = query(None, None, Some(1));

    let window = filter
        .apply_reader(Cursor::new(content), "test.md")
        .expect("window outside the invalid bytes should decode");

    assert_eq!(window, "# Heading\n");
}

#[test]
fn invalid_utf8_inside_the_window_is_an_error() {
    let filter = query(None, None, Some(2));

    let result = filter.apply_reader(Cursor::new(vec![b'a', b'\n', 0xff, b'\n']), "test.md");

    assert!(matches!(
        result,
        Err(crate::error::AppError::InvalidUtf8 { .. })
    ));
}

// --- Wire format validation ---------------------------------------------

#[track_caller]
fn query_rejects(from: Option<&str>, to: Option<&str>, maximum: Option<usize>) -> String {
    let error = SeekOptions {
        seek_from_line_starts_with: from.map(str::to_string),
        seek_to_line_starts_with: to.map(str::to_string),
        seek_lines_maximum: maximum,
    }
    .parse()
    .expect_err("seek options should have been rejected");

    error.to_string()
}

#[test]
fn query_prefix_lists_accept_only_the_json_array_spelling() {
    // Query parameters are strings, so there is exactly one canonical
    // spelling — no bare string, no malformed JSON, no non-string elements.
    for raw in ["---", "[", "[1]", "{}", "\"---\""] {
        query_rejects(Some(raw), None, None);
    }

    assert!(query(Some(r#"["---"]"#), None, None)
        .from_prefixes
        .is_some());
}

#[test]
fn query_rejects_empty_arrays_empty_prefixes_and_a_zero_maximum() {
    // An empty prefix would match every line and an empty array none: both
    // can only be caller bugs, so neither is silently ignored.
    assert!(query_rejects(Some("[]"), None, None).contains("at least one prefix"));
    assert!(query_rejects(Some(r#"[""]"#), None, None).contains("must not be empty"));
    assert!(query_rejects(None, Some("[]"), None).contains("at least one prefix"));
    assert!(query_rejects(None, None, Some(0)).contains("at least 1"));
}

#[test]
fn the_meta_operator_needs_a_from_filter_to_resolve() {
    let reason = query_rejects(None, Some("$seek_from_line_starts_with"), None);

    assert!(
        reason.contains("seek_from_line_starts_with is not set"),
        "{}",
        reason
    );

    // Also inside an array element, not just bare.
    query_rejects(None, Some(r#"["$seek_from_line_starts_with"]"#), None);
}

#[test]
fn the_bare_meta_operator_is_the_only_unwrapped_to_value() {
    let filter = query(
        Some(r##"["#"]"##),
        Some("$seek_from_line_starts_with"),
        None,
    );

    assert_eq!(
        filter.to_prefixes.as_deref(),
        Some(["$seek_from_line_starts_with".to_string()].as_slice())
    );

    // Any other bare string must still be array-wrapped.
    query_rejects(Some(r##"["#"]"##), Some("---"), None);
}

#[test]
fn the_body_spelling_takes_native_arrays_and_the_bare_meta_operator() {
    let filter = SeekBody {
        from_line_starts_with: Some(vec!["---".to_string()]),
        to_line_starts_with: Some(crate::seek::SeekBodyToPrefixes::Value(
            "$seek_from_line_starts_with".to_string(),
        )),
        lines_maximum: Some(3),
    }
    .parse()
    .expect("body seek rejected");

    assert_eq!(apply(&filter, FRONT_MATTER), "---\ntitle: Hello\n---\n");
}

#[test]
fn the_body_spelling_rejects_any_other_bare_string() {
    let error = SeekBody {
        from_line_starts_with: None,
        to_line_starts_with: Some(crate::seek::SeekBodyToPrefixes::Value("---".to_string())),
        lines_maximum: None,
    }
    .parse()
    .expect_err("bare string should have been rejected");

    // The error names the field as the caller spelled it in the body, not
    // as the query parameter is spelled.
    assert!(
        error.to_string().contains("seek.to_line_starts_with"),
        "{}",
        error
    );
}

#[test]
fn both_wire_formats_share_one_validation_funnel() {
    // Same rejection, both spellings: an empty prefix list.
    let body_error = SeekBody {
        from_line_starts_with: Some(vec![]),
        to_line_starts_with: None,
        lines_maximum: None,
    }
    .parse()
    .expect_err("empty body array should have been rejected");

    assert!(body_error.to_string().contains("at least one prefix"));
    assert!(query_rejects(Some("[]"), None, None).contains("at least one prefix"));
}
