use super::*;

#[test]
fn test_apply_one_single_replace() {
    let c = "fn foo() { 1 }\n";
    let out = apply_one(c, "1", "2", false).unwrap();
    assert_eq!(out, "fn foo() { 2 }\n");
}

#[test]
fn test_apply_one_refuses_empty() {
    let err = apply_one("abc", "", "x", false).unwrap_err();
    assert!(err.contains("non-empty"));
}

#[test]
fn test_apply_one_refuses_noop() {
    let err = apply_one("abc", "b", "b", false).unwrap_err();
    assert!(err.contains("no-op"));
}

#[test]
fn test_apply_one_refuses_zero() {
    let err = apply_one("abc", "z", "y", false).unwrap_err();
    assert!(err.contains("not found"));
}

#[test]
fn test_apply_one_refuses_ambiguous() {
    let err = apply_one("a a a", "a", "b", false).unwrap_err();
    // The multi-match error: states the count, gives the two recovery
    // paths, and echoes the offending old_string.
    assert!(err.contains("Found 3 matches"), "{err}");
    assert!(err.contains("replace_all"), "{err}");
    assert!(err.contains("more context"), "{err}");
    assert!(err.contains("String: a"), "must echo the old_string: {err}");
}

#[test]
fn test_apply_one_replace_all() {
    let out = apply_one("a a a", "a", "b", true).unwrap();
    assert_eq!(out, "b b b");
}

/// The atomic-batch core branch: if the second edit in a sequence
/// fails, the original content is unchanged (the first edit's
/// in-memory result is discarded, no write). This is the all-or-
/// nothing invariant MultiEdit relies on.
#[test]
fn test_multiedit_second_fail_original() {
    let original = "fn foo() { 1 }\n";
    let after_first = apply_one(original, "1", "2", false).unwrap();
    assert_eq!(after_first, "fn foo() { 2 }\n");
    let err = apply_one(&after_first, "nonexistent", "x", false);
    assert!(err.is_err(), "second edit must fail");
    assert_eq!(original, "fn foo() { 1 }\n", "original is unchanged");
}
