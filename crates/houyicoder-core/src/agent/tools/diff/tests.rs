//! Tests for this module.

use super::*;

#[test]
fn test_diff_identical_empty() {
    assert_eq!(unified_diff("a\nb\nc", "a\nb\nc", 3), "");
}

#[test]
fn test_diff_single_replace() {
    let orig = "fn foo() {\n    1\n}\n";
    let new = "fn foo() {\n    2\n}\n";
    let d = unified_diff(orig, new, 1);
    assert!(d.contains("@@"));
    assert!(d.contains("-    1"));
    assert!(d.contains("+    2"));
}

#[test]
fn test_diff_insert_only() {
    let d = unified_diff("a\nb", "a\nx\nb", 1);
    assert!(d.contains("+x"));
    assert!(!d.contains("-a"));
    assert!(!d.contains("-b"));
}

#[test]
fn test_diff_delete_only() {
    let d = unified_diff("a\nx\nb", "a\nb", 1);
    assert!(d.contains("-x"));
    assert!(!d.contains("+a"));
    assert!(!d.contains("+b"));
}

#[test]
fn test_diff_multi_hunk_separated() {
    // Two far-apart changes must produce TWO hunks (Myers), not one merged
    // block with duplicated unchanged middle lines (the old hand-rolled
    // single-hunk bug).
    let orig = "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n";
    let new = "L1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nL10\n";
    let d = unified_diff(orig, new, 1);
    let hunk_count = d.matches("@@").count();
    assert!(hunk_count >= 2, "expected >=2 hunks, got {hunk_count}\n{d}");
    // Middle unchanged lines must not be duplicated as both - and +.
    assert!(!d.contains("-l5\n+l5"));
}

#[test]
fn test_diff_no_newline_marker() {
    // A file without a trailing newline, edited to add one: the
    // No newline at end of file marker must appear.
    let d = unified_diff("a\nb", "a\nb\n", 3);
    assert!(
        d.contains("No newline"),
        "expected no-newline marker, got:\n{d}"
    );
}

#[test]
fn test_diff_crlf_normalized() {
    // CRLF input must not leak CR into the hunk body.
    let d = unified_diff("a\r\nb\r\n", "a\r\nx\r\n", 1);
    assert!(!d.contains('\r'), "CRLF leaked into diff:\n{d}");
    assert!(d.contains("+x"));
}

#[test]
fn test_diff_strips_file_header() {
    let d = unified_diff("a\n", "b\n", 1);
    assert!(!d.starts_with("--- "), "file header not stripped:\n{d}");
    assert!(!d.contains("+++ "));
}
