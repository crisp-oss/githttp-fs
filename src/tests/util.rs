// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! Unit tests for `util.rs`.
//!
//! `constant_time_eq` is the comparison behind both credential guards, so it
//! is tested for correctness here; its timing property is a property of the
//! implementation (no early return, one accumulator) rather than something a
//! test can assert reliably on a shared CI machine.

use crate::util::constant_time_eq;

#[test]
fn equal_slices_compare_equal() {
    assert!(constant_time_eq(b"", b""));
    assert!(constant_time_eq(b"secret", b"secret"));
    assert!(constant_time_eq(&[0u8, 255, 128], &[0u8, 255, 128]));
}

#[test]
fn any_difference_compares_unequal() {
    // Including a difference in the first byte and in the last, since the
    // point of the function is that neither short-circuits.
    assert!(!constant_time_eq(b"secret", b"Secret"));
    assert!(!constant_time_eq(b"secret", b"secreT"));
    assert!(!constant_time_eq(b"secret", b"secrez"));
}

#[test]
fn different_lengths_compare_unequal() {
    // The length check is the one comparison allowed to short-circuit: the
    // length of the configured key is not considered secret.
    assert!(!constant_time_eq(b"secret", b"secret-longer"));
    assert!(!constant_time_eq(b"secret", b"secre"));
    assert!(!constant_time_eq(b"", b"x"));
}
