//! Tests for this module.

use super::*;

#[test]
fn test_plain_command_word() {
    assert_eq!(simple_command_word("mv a b"), Some("mv"));
    assert_eq!(simple_command_word("  grep -n foo  "), Some("grep"));
}

#[test]
fn test_env_prefix_stripped() {
    assert_eq!(simple_command_word("FOO=bar mv a b"), Some("mv"));
    assert_eq!(simple_command_word("A=1 B=2 grep x"), Some("grep"));
}

/// A value holding an equals sign must not swallow the command word.
#[test]
fn test_env_value_with_equals() {
    assert_eq!(simple_command_word("URL=a=b curl x"), Some("curl"));
}

/// A token that only looks like an assignment (not a valid name) ends
/// the prefix scan, so it is treated as the command word itself.
#[test]
fn test_non_name_assignment_stops() {
    assert_eq!(simple_command_word("a-b=1 mv x y"), Some("a-b=1"));
}

#[test]
fn test_compound_rejected() {
    assert_eq!(simple_command_word("grep x | head"), None);
    assert_eq!(simple_command_word("mv a b && echo ok"), None);
    assert_eq!(simple_command_word("mv a b; ls"), None);
    assert_eq!(simple_command_word("mv a b || true"), None);
}

#[test]
fn test_empty_is_none() {
    assert_eq!(simple_command_word(""), None);
    assert_eq!(simple_command_word("   "), None);
}
