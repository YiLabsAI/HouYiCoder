//! The keychain output parser, and the process handling driven by a
//! trivial child.

use super::{Lookup, lookup, parse_security_output, query_args};
use crate::host::secrets::SecretRead;

/// The lookup names the item by service and account and asks for the
/// password alone.
#[test]
fn test_query_names_item() {
    assert_eq!(
        query_args("houyicoder", "dashscope"),
        [
            "find-generic-password",
            "-s",
            "houyicoder",
            "-a",
            "dashscope",
            "-w"
        ]
    );
}

/// The tool's line terminator is removed and the password is the key.
#[test]
fn test_output_terminator_stripped() {
    assert_eq!(
        parse_security_output(b"sk-1\n"),
        SecretRead::Value("sk-1".into())
    );
}

/// Neither a line ending nor a stray byte around the value is repaired: a
/// value with anything but the one terminator is reported as unusable.
#[test]
fn test_output_padding_rejects() {
    assert_eq!(parse_security_output(b" sk-1\n"), SecretRead::InvalidValue);
    assert_eq!(parse_security_output(b"sk-1\n\n"), SecretRead::InvalidValue);
}

/// Empty output is not a key, even though the tool exited cleanly.
#[test]
fn test_output_empty_rejects() {
    assert_eq!(parse_security_output(b""), SecretRead::InvalidValue);
}

/// A child that succeeds hands its output back.
#[test]
fn test_lookup_returns_output() {
    match lookup("/bin/sh", &["-c", "printf sk-1"]) {
        Lookup::Output(bytes) => assert_eq!(bytes, b"sk-1"),
        _ => panic!("a clean run reports its output"),
    }
}

/// A child that fails is reported as a missing item, which is how the tool
/// answers both a missing entry and a denied authorization.
#[test]
fn test_lookup_reports_failure() {
    assert!(matches!(
        lookup("/bin/sh", &["-c", "exit 44"]),
        Lookup::Missing
    ));
}

/// A program that cannot start is reported as a failed lookup rather than as
/// a missing item: the two need different notices.
#[test]
fn test_lookup_reports_unrunnable() {
    assert!(matches!(
        lookup("/nonexistent/security", &[]),
        Lookup::Failed
    ));
}
