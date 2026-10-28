//! Credential paths, shared by the two layers that have to recognise them:
//! the sandbox profile, which denies them at the kernel, and the permission
//! gate, which must not waive an approval for a command that reads one.

/// Credential and shell-startup files. Ordinary source files stay readable
/// across projects, but a credential cannot rely on that broad read policy
/// until value redaction exists, so reading or writing one is always judged
/// separately. A workspace .env file stays readable as a development input,
/// which is why it is not listed.
pub const CREDENTIAL_FILES: &[&str] = &[
    ".bashrc",
    ".bash_profile",
    ".zshrc",
    ".zprofile",
    ".profile",
    ".credentials.json",
    ".mcp.json",
    ".netrc",
    ".npmrc",
    ".pypirc",
];

/// Home directories that hold credentials or the agent definitions that
/// decide what runs later, so reading one leaks a secret and writing one
/// hijacks a future session.
pub const CREDENTIAL_DIRS: &[&str] = &[
    ".ssh",
    ".claude/commands",
    ".claude/agents",
    ".aws",
    ".gnupg",
    ".kube",
    ".docker",
];

/// Whether a command line or path names one of the credential paths above.
/// Matching is on whole path components after normalizing separators, so a
/// file whose name merely contains a longer name does not match, while a copy
/// carrying an extra suffix does.
pub fn names_credential_path(content: &str) -> bool {
    let lower = content.replace('\\', "/").to_ascii_lowercase();
    CREDENTIAL_FILES
        .iter()
        .any(|name| names_component(&lower, name, false))
        || CREDENTIAL_DIRS
            .iter()
            .any(|name| names_component(&lower, name, true))
}

/// Whether the text contains the name as a path component. A directory name
/// either ends its word or continues into the directory with a separator; a
/// file name is closed by any character that cannot continue a name.
fn names_component(lower: &str, name: &str, directory: bool) -> bool {
    let mut from = 0;
    while let Some(at) = lower[from..].find(name) {
        let start = from + at;
        let end = start + name.len();
        let before = lower[..start].chars().next_back();
        let after = lower[end..].chars().next();
        let opens = before.is_none_or(|c| !is_name_char(c));
        let closes = if directory {
            after.is_none_or(|c| c == '/' || c.is_whitespace() || ends_word(c))
        } else {
            after.is_none_or(|c| !is_name_char(c))
        };
        if opens && closes {
            return true;
        }
        from = start + 1;
    }
    false
}

/// Whether the character ends a shell word, so a directory path written
/// without a trailing separator still names the directory.
fn ends_word(c: char) -> bool {
    matches!(c, '\'' | '"' | '`' | ';' | '&' | '|' | ')' | '<' | '>')
}

/// Whether the character can continue a path component. A dot is excluded so
/// a backup copy of a credential file still names the credential.
fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-')
}

#[cfg(test)]
mod tests;
