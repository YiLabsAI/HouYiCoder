//! Tests for the detection stage.

use super::interpreter_inline_code;

/// A quote the code escapes belongs to the code, not to its end. Reading
/// the first match would hand back a prefix that looks clean while the
/// rest of the body still runs.
#[test]
fn test_inline_code_escaped_quote() {
    let content = "python3 -c \"x=1;y=\\\"a\\\";import os\"";
    assert_eq!(
        interpreter_inline_code(content),
        Some("x=1;y=\\\"a\\\";import os"),
        "the escaped quote stays inside the body"
    );
}

/// A single-quoted body has no escapes, so a backslash is a character.
#[test]
fn test_inline_code_single_quoted() {
    let content = "python3 -c 'print(1)'";
    assert_eq!(interpreter_inline_code(content), Some("print(1)"));
}
