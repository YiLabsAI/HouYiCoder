//! Tests for the detection stage.

use super::{interpreter_inline_code, should_ask_destructive};

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

/// A search pattern that contains a destructive verb inside a longer
/// identifier is not a destructive command. The scan split on the underscore,
/// so a read-only search for rm_targets_are_temp produced the bare verb rm and
/// the gate asked about a call that could not delete anything.
#[test]
fn test_identifier_is_not_destructive() {
    assert!(
        !should_ask_destructive(
            "bash",
            "grep -n \"is_rm_targets_are_temp\\|rm_targets_are_temp\" -r crates/ | head -30"
        ),
        "a search for an identifier that contains rm is not a delete"
    );
    assert!(
        !should_ask_destructive("bash", "rg \"truncate_width\""),
        "a search for an identifier that contains truncate is not a truncate"
    );
}

/// The verbs themselves still escalate, including the forms a wrapper or a
/// shell can carry, so narrowing the split did not widen the allow path.
#[test]
fn test_destructive_verbs_still_ask() {
    for command in [
        "rm -rf build",
        "rmdir empty",
        "sh -c 'rm file'",
        "sudo apt install x",
        "dd if=/dev/zero of=/dev/null",
        "truncate -s 0 log",
    ] {
        assert!(
            should_ask_destructive("bash", command),
            "a destructive command must still escalate: {command}"
        );
    }
}
