//! Credential path recognition: the shapes a command line or path can take.

use super::names_credential_path;

/// A home path, an absolute path, and a nested one all name the credential.
#[test]
fn test_names_credential_file() {
    for text in [
        "cat ~/.netrc",
        "cat /home/u/.npmrc",
        "head -n 1 .pypirc",
        "cat ~/.ssh/id_rsa",
        "ls ~/.aws",
        "cat ~/.claude/.credentials.json",
    ] {
        assert!(names_credential_path(text), "{text}");
    }
}

/// Project configuration files that name commands are not credentials: they
/// carry no secret, and a sandboxed session reads them to list servers.
#[test]
fn test_config_files_not_credentials() {
    assert!(!names_credential_path("cat .mcp.json"));
    assert!(!names_credential_path("cat .lsp.json"));
}

/// A name that merely contains a longer name is a different file, so it does
/// not count; a backup copy of the credential does.
#[test]
fn test_boundary_is_component() {
    assert!(!names_credential_path("cat ./install.bashrc"));
    assert!(!names_credential_path("cat my.netrc.list"));
    assert!(!names_credential_path("cat .sshrc"));
    assert!(names_credential_path("cat ~/.netrc.bak"));
}

/// A Windows-style path reaches the same file.
#[test]
fn test_backslash_path_matches() {
    assert!(names_credential_path(r"type C:\Users\u\.ssh\id_rsa"));
}

/// A directory written without a trailing separator still names it, both as
/// the last word and before another separator.
#[test]
fn test_directory_ends_word() {
    assert!(names_credential_path("cp -r .ssh /tmp/backup"));
    assert!(names_credential_path("ls ~/.aws; echo done"));
}

/// Ordinary work names no credential.
#[test]
fn test_ordinary_command_no_match() {
    for text in ["ls -la /tmp", "git status", "rg ssh src/"] {
        assert!(!names_credential_path(text), "{text}");
    }
}
