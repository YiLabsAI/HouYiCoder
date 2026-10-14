//! Shell command-line inspection shared by the decisions that key off which
//! command ran: the semantic-exit judgment (a non-zero exit that is not a
//! failure) and the silent-success judgment (an empty output that means done).
//! Both need the same thing — the command word of a simple command — and both
//! must refuse the same thing: a compound command, whose exit code and output
//! belong to the last stage rather than the first word. One implementation so
//! the two judgments cannot disagree about what command a line ran.

/// The command word of a simple command, or None when the line is compound.
///
/// A pipeline or a chain (pipe, semicolon, and-and, or-or) yields None: the
/// exit code and the output of such a line come from its last stage, so
/// attributing either to the first word would misread the result. "grep x |
/// head" exiting 1 means head failed, not that grep found no match.
///
/// Leading environment assignments are stripped, so FOO=bar mv a b reports
/// mv. POSIX allows any number of them before the command word.
pub fn simple_command_word(command: &str) -> Option<&str> {
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
mod tests;
