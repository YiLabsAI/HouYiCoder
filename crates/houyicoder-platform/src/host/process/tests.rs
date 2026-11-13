//! The command-line builder, over the quoting edges the kernel parser applies.

use super::{build_cmd_line, build_command_line};

/// A plain argument with no special character passes through unchanged.
#[test]
fn test_plain_argument_unquoted() {
    assert_eq!(
        build_command_line("prog", &["alpha", "beta"]),
        "prog alpha beta"
    );
}

/// An empty argument is quoted, so the child sees an empty token rather than
/// nothing.
#[test]
fn test_empty_argument_quoted() {
    assert_eq!(build_command_line("prog", &[""]), "prog \"\"");
}

/// An argument with a space is wrapped, so the space does not split it.
#[test]
fn test_space_argument_quoted() {
    assert_eq!(build_command_line("prog", &["a b", "c"]), "prog \"a b\" c");
}

/// A program path with a space is quoted as the first token.
#[test]
fn test_program_with_space_quoted() {
    assert_eq!(
        build_command_line("C:\\Program Files\\app.exe", &["--flag"]),
        "\"C:\\Program Files\\app.exe\" --flag"
    );
}

/// A quote inside an argument is escaped, and the backslashes before it double.
#[test]
fn test_quote_argument_escaped() {
    assert_eq!(build_command_line("prog", &["a\"b"]), "prog \"a\\\"b\"");
}

/// A backslash run before a quote doubles; a backslash run not before a quote
/// stays single.
#[test]
fn test_backslashes_before_quote() {
    assert_eq!(
        build_command_line("prog", &["a\\\\\"b"]),
        "prog \"a\\\\\\\\\\\"b\""
    );
}

/// A trailing backslash run before the closing quote doubles, so the child does
/// not read it as escaping the quote. The argument has a space so it is quoted.
#[test]
fn test_trailing_backslashes_double() {
    assert_eq!(build_command_line("prog", &["a b\\"]), "prog \"a b\\\\\"");
}

/// A path argument with backslashes but no space needs no quoting, and its
/// backslashes stay single.
#[test]
fn test_path_without_space_unquoted() {
    assert_eq!(
        build_command_line("prog", &["C:\\dir\\file.txt"]),
        "prog C:\\dir\\file.txt"
    );
}

/// The command interpreter command line wraps the interpreter and the raw
/// command in one pair of quotes each, behind the run flags, and never
/// backslash-escapes the inner quotes the raw command carries.
#[test]
fn test_cmd_line_wraps_command() {
    assert_eq!(
        build_cmd_line("cmd.exe", "echo hello"),
        "\"cmd.exe\" /d /s /c \"echo hello\""
    );
}

/// A quote inside the raw command reaches the interpreter as a quote, not as a
/// backslash-quote: the interpreter has its own parser and a backslash would
/// land in the output as a literal character.
#[test]
fn test_cmd_line_keeps_quotes() {
    assert_eq!(
        build_cmd_line("C:\\Windows\\System32\\cmd.exe", "echo \"hello\""),
        "\"C:\\Windows\\System32\\cmd.exe\" /d /s /c \"echo \"hello\"\""
    );
}
