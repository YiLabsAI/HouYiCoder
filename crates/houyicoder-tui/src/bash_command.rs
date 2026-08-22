//! Shell command-line inspection shared by the two render-layer decisions
//! that key off which command ran: the semantic-exit judgment (a non-zero
//! exit that is not a failure) and the silent-success judgment (an empty
//! output that means done). Both need the same thing — the command word of
//! a simple command — and both must refuse the same thing: a compound
//! command, whose exit code and output belong to the last stage rather than
//! the first word. One implementation so the two judgments cannot disagree
//! about what command a line ran.

/// The command word of a simple command, or None when the line is compound.
///
/// A pipeline or a chain (pipe, semicolon, and-and, or-or) yields None: the
/// exit code and the output of such a line come from its last stage, so
/// attributing either to the first word would misread the result. "grep x |
/// head" exiting 1 means head failed, not that grep found no match.
///
/// Leading environment assignments are stripped, so FOO=bar mv a b reports
/// mv. POSIX allows any number of them before the command word.
pub(crate) fn simple_command_word(command: &str) -> Option<&str> {
    let trimmed = command.trim();
    if trimmed.contains('|')
        || trimmed.contains(';')
        || trimmed.contains("&&")
        || trimmed.contains("||")
    {
        return None;
    }
    let word = strip_env_prefix(trimmed).split_whitespace().next()?;
    if word.is_empty() { None } else { Some(word) }
}

/// Strip leading VAR=value assignments so the command word is reachable.
/// Stops at the first token that is not a well-formed assignment, so a bare
/// word or a value containing an equals sign does not consume the command.
fn strip_env_prefix(cmd: &str) -> &str {
    let mut rest = cmd;
    loop {
        let trimmed = rest.trim_start();
        let Some(eq) = trimmed.find('=') else {
            return trimmed;
        };
        let before = &trimmed[..eq];
        if before.is_empty()
            || !before
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return trimmed;
        }
        let after = &trimmed[eq + 1..];
        // The value runs to the next whitespace.
        let end = after.find(char::is_whitespace).unwrap_or(after.len());
        rest = &after[end..];
    }
}

#[cfg(test)]
mod tests {
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
}
