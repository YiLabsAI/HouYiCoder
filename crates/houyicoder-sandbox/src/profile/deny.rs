//! Mandatory-deny policy for the seatbelt profile: the exfiltration and
//! self-authorization vectors an AI agent must not read or write, plus the
//! rule-emitting builders that apply them. Split from profile.rs so the
//! policy tables and their emit loop live as one module, leaving profile.rs
//! to the workspace / allow-set / fs / network composition.
//!
//! These denies land AFTER the fs allow-back in render(), so seatbelt's
//! last-match-wins cannot wash them out. See profile.rs::render for the
//! segment order, which is load-bearing.

use std::path::Path;

/// AI-agent exfiltration vectors — files. Read+write denied so an agent can
/// neither exfiltrate credentials nor plant a malicious rc file that fires
/// when the user later opens a shell.
///
/// The credential files (.netrc / .npmrc / .pypirc) are denied outright
/// rather than left to consent and the model's restraint. With the /Users
/// broad read-deny lifted (so cross-project source reads work), these home
/// credential files would otherwise become readable. The stance is
/// deliberately stricter on credentials and looser on ordinary source. The
/// terminal posture replaces these read-denies with value-level redaction so
/// the file stays readable while the secret is swapped on the way into the
/// transcript; until that lands, the deny is the safe interim.
const MANDATORY_DENY_FILES: &[&str] = &[
    ".bashrc",
    ".bash_profile",
    ".zshrc",
    ".zprofile",
    ".profile",
    ".mcp.json",
    // Credential files — home-only, read+write denied as the safe interim
    // until redaction. .env is intentionally NOT here: a workspace .env is
    // normal dev input and must stay readable; its secrets are a redaction
    // target, not a read-deny target.
    ".netrc",
    ".npmrc",
    ".pypirc",
];

/// Vectors denied for write only, with reads allowed back further down. The
/// home git config, the repo .git/config, .gitmodules, and .ripgreprc are
/// the members. git reads the configs on every invocation (log, diff,
/// status) and .gitmodules on submodule ops; rg reads .ripgreprc on
/// startup. Denying any of those reads breaks the tool outright. The config
/// files may hold a credential section (interim read-allow until
/// redaction), while .gitmodules and .ripgreprc carry no secrets (submodule
/// URLs and rg flags). Write stays denied for all: a writable git config is
/// a credential-helper redirection vector, a writable .gitmodules plants a
/// malicious submodule URL that fires on the next submodule update, and a
/// writable .ripgreprc plants rg flags.
///
/// This is an interim posture, not the end state. Allowing the read means a
/// credential section in that file can reach the transcript, which is an
/// unrecoverable outward send. The end state is value-level redaction of the
/// credential entries on the way into the transcript, which keeps the file
/// readable and faithful while hiding the secret. Until that lands, the
/// residual risk is accepted deliberately and is registered as such.
const MANDATORY_DENY_WRITE_ONLY_FILES: &[&str] =
    &[".gitconfig", ".git/config", ".gitmodules", ".ripgreprc"];

/// Files that record what the user has consented to, or that the fence itself
/// is built from. Write denied wherever they appear.
///
/// These are not exfiltration vectors, they are self-authorization vectors, and
/// the distinction is why they need their own list. A run that appends to the
/// persisted permission rules grants itself standing approval for every later
/// turn, and a rule sitting in that file is indistinguishable from one the user
/// added. A run that rewrites the settings widens the fence its own next
/// invocation is rendered from. Either way the escalation survives the turn that
/// performed it, which is what separates this from a one-shot denied operation.
///
/// Matched by name wherever they occur rather than at one absolute location,
/// because the permission store resolves two of its three scopes relative to
/// the working directory and the temp directory, both of which are writable by
/// design. Reads stay allowed: knowing which rules exist is not the hazard,
/// authoring them is.
///
/// Deliberately narrow. Denying writes to the whole config directory would be
/// the broader guard, but a session can legitimately run with its workspace
/// inside that directory, since worktrees are created there, and a blanket deny
/// would then refuse the agent every write in its own workspace.
const MANDATORY_DENY_AUTHORITY_FILES: &[&str] = &[
    ".houyicoder/permissions.json",
    ".houyicoder/settings.json",
    "houyicoder-permissions/permissions.json",
];

/// AI-agent exfiltration vectors — directories (srt DANGEROUS_DIRECTORIES +
/// the CLI command/agent dirs). .ssh holds private keys; the CLI dirs hold
/// agent/skill definitions an attacker could rewrite to hijack later runs.
/// The credential directories (.aws / .gnupg / .kube / .docker) are added
/// here for the same reason as the credential files above: once the /Users
/// broad read-deny is lifted, these home credential dirs would otherwise be
/// readable, and a deny is the safe interim until redaction lands.
const MANDATORY_DENY_DIRS: &[&str] = &[
    ".ssh",
    ".claude/commands",
    ".claude/agents",
    ".aws",
    ".gnupg",
    ".kube",
    ".docker",
];

/// Git-internal directories denied write only, reads allowed back further
/// down — same posture as the git config entries above. .git/hooks blocks
/// hook-based escape (an agent planting a post-checkout hook that runs when
/// the user next checks out); the escape is a write, and reading hook source
/// is not the vector. A hook may hold a hardcoded secret just as config may
/// hold a credential section, so both reads are accepted under the same
/// interim posture until value-level redaction lands.
const MANDATORY_DENY_GIT_PATHS: &[&str] = &[".git/hooks"];

/// Mandatory deny of AI-agent exfiltration vectors (srt
/// macos-sandbox-utils:60-86 + the CLI the CLI skills denyWrite idea). For each
/// mandatory file/dir/git-path, deny read AND write of both the home literal
/// copy and any nested-anywhere copy (regex). Landing after filesystem_rules
/// means an allow-back of the workspace cannot wash these out — this is the
/// denyReadAlways principle applied to the exfiltration vectors themselves.
///
/// Regex uses ICU syntax as accepted by sandbox-exec. For a file name we
/// match it as the final path segment; for a directory we match the segment
/// followed by end-of-string or a separator.
pub(crate) fn mandatory_deny(home: &str, tag: &str) -> String {
    let mut out = String::new();
    let emit = |out: &mut String, subpath: &str, regex: &str| {
        out.push_str(&format!(
            "(deny file-read* file-write* (subpath \"{subpath}\") (regex #\"{regex}\") (with message \"{tag}\"))\n"
        ));
    };
    for file in MANDATORY_DENY_FILES {
        let escaped = file.replace('.', r"\.");
        emit(
            &mut out,
            &format!("{home}/{file}"),
            &format!(".*/{escaped}$"),
        );
    }
    // Write-only members: same two shapes (home literal plus nested-anywhere
    // regex), but the read verb is omitted so the read allow-back further down
    // can take effect. Omitting the read here is what makes the allow-back
    // possible at all: a read deny in this segment would be evaluated after
    // the allow-back only if it were emitted later, and moving it later would
    // defeat the allow-back entirely.
    for file in MANDATORY_DENY_WRITE_ONLY_FILES {
        let escaped = file.replace('.', r"\.");
        let subpath = format!("{home}/{file}");
        out.push_str(&format!(
            "(deny file-write* (subpath \"{subpath}\") (regex #\".*/{escaped}$\") (with message \"{tag}\"))\n"
        ));
    }
    // Authority files: write denied, read left alone. The nested-anywhere regex
    // is the load-bearing half, because the scopes that were reachable are the
    // ones resolved against the working and temp directories rather than the
    // home literal.
    for file in MANDATORY_DENY_AUTHORITY_FILES {
        let escaped = file.replace('.', r"\.");
        let subpath = format!("{home}/{file}");
        out.push_str(&format!(
            "(deny file-write* (subpath \"{subpath}\") (regex #\".*/{escaped}$\") (with message \"{tag}\"))\n"
        ));
    }
    for dir in MANDATORY_DENY_DIRS {
        let escaped = dir.replace('.', r"\.");
        emit(
            &mut out,
            &format!("{home}/{dir}"),
            &format!(".*/{escaped}(/|$)"),
        );
    }
    for git_path in MANDATORY_DENY_GIT_PATHS {
        let escaped = git_path.replace('.', r"\.");
        // Write-only (read verb omitted) so the read allow-back further
        // down can take effect; the hooks escape is a write, not a read.
        out.push_str(&format!(
            "(deny file-write* (subpath \"{home}/{git_path}\") (regex #\".*/{escaped}(/|$)\") (with message \"{tag}\"))\n"
        ));
    }
    out
}

/// Deny writes to the snapshot store directory inside the workspace. This
/// prevents a destructive command (rm -rf, git clean -fdx) from deleting
/// its own undo data. The snapshot store is written by the host process
/// (BashTool::execute before exec), not by the sandboxed child, so this deny
/// does not block snapshot creation — it only blocks the agent's commands from
/// clobbering the undo stack.
pub(crate) fn deny_snapshot_store(workspace: &Path, tag: &str) -> String {
    let ws = workspace.to_string_lossy();
    format!(
        "(deny file-write* (subpath \"{ws}/.houyicoder/snapshots\") (with message \"{tag}\"))\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mandatory_deny_home_paths() {
        let p = mandatory_deny("/Users/alice", "tag-x");
        assert!(p.contains("(subpath \"/Users/alice/.ssh\")"));
        assert!(p.contains("(subpath \"/Users/alice/.gitconfig\")"));
        assert!(p.contains("(subpath \"/Users/alice/.mcp.json\")"));
        assert!(p.contains("(subpath \"/Users/alice/.bashrc\")"));
        assert!(p.contains("(subpath \"/Users/alice/.zshrc\")"));
        assert!(p.contains("(subpath \"/Users/alice/.profile\")"));
        assert!(p.contains("(subpath \"/Users/alice/.claude/commands\")"));
        assert!(p.contains("(subpath \"/Users/alice/.claude/agents\")"));
    }

    #[test]
    fn test_git_hooks_write_denied() {
        let p = mandatory_deny("/Users/alice", "tag-x");
        assert!(p.contains("(subpath \"/Users/alice/.git/hooks\")"));
        assert!(p.contains(r".*/\.git/hooks(/|$)"));
        assert!(
            p.contains("(deny file-write* (subpath \"/Users/alice/.git/hooks\")"),
            "hooks must be write-denied: {p}"
        );
        assert!(
            !p.contains("(deny file-read* file-write* (subpath \"/Users/alice/.git/hooks\")"),
            "hooks must not be read-denied, same posture as git config: {p}"
        );
    }

    #[test]
    fn test_mandatory_deny_authority_files() {
        let p = mandatory_deny("/Users/alice", "tag-x");
        assert!(p.contains(r".*/\.houyicoder/permissions\.json$"));
        assert!(p.contains(r".*/\.houyicoder/settings\.json$"));
        assert!(p.contains(r".*/houyicoder-permissions/permissions\.json$"));
        assert!(p.contains("(subpath \"/Users/alice/.houyicoder/permissions.json\")"));
        let line = p
            .lines()
            .find(|l| l.contains(r".*/\.houyicoder/permissions\.json$"))
            .expect("authority deny present");
        assert!(
            line.starts_with("(deny file-write*"),
            "authority files are write-denied only, since authoring rules is the \
             hazard and reading them is not: {line}"
        );
    }

    /// The guard must not widen into a directory deny. Worktrees are created
    /// under the same directory, so a session can be working inside it, and a
    /// deny written against the directory would refuse the agent every write in
    /// its own workspace.
    #[test]
    fn test_authority_deny_stays_narrow() {
        let p = mandatory_deny("/Users/alice", "tag-x");
        assert!(
            !p.contains(r".*/\.houyicoder(/|$)"),
            "a directory-level deny of the config directory breaks worktree \
             sessions, which run with their workspace inside it"
        );
    }

    #[test]
    fn test_mandatory_denies_read_write() {
        let p = mandatory_deny("/Users/alice", "tag-x");
        assert!(p.contains("deny file-read* file-write*"));
    }

    /// The home git config is denied for write only. Read is allowed back
    /// further down so git works; write stays denied because a writable git
    /// config is a credential-helper injection vector. Every other mandatory
    /// entry keeps denying both verbs.
    #[test]
    fn test_gitconfig_write_denied() {
        let p = mandatory_deny("/Users/alice", "tag-x");
        assert!(
            p.contains("(deny file-write* (subpath \"/Users/alice/.gitconfig\")"),
            "home git config must be write-denied: {p}"
        );
        assert!(
            !p.contains("(deny file-read* file-write* (subpath \"/Users/alice/.gitconfig\")"),
            "home git config must not be read-denied, the read allow-back depends on it: {p}"
        );
        assert!(
            p.contains("(deny file-read* file-write* (subpath \"/Users/alice/.ssh\")"),
            "other mandatory entries must still deny both verbs: {p}"
        );
    }

    /// The repo .git/config is denied for write only, same posture as the
    /// home git config above: git reads it on every log/diff/status, so a
    /// read deny breaks daily git; write stays denied because a writable
    /// config is a credential-helper injection vector.
    #[test]
    fn test_git_config_write_denied() {
        let p = mandatory_deny("/Users/alice", "tag-x");
        assert!(
            p.contains("(deny file-write* (subpath \"/Users/alice/.git/config\")"),
            "repo .git/config must be write-denied: {p}"
        );
        assert!(
            !p.contains("(deny file-read* file-write* (subpath \"/Users/alice/.git/config\")"),
            "repo .git/config must not be read-denied, git needs to read it: {p}"
        );
    }

    /// .gitmodules is denied for write only: its contents are submodule
    /// URLs (not secrets), so reads are safe, and the danger is planting a
    /// malicious submodule URL that fires on the next submodule update.
    #[test]
    fn test_gitmodules_write_denied() {
        let p = mandatory_deny("/Users/alice", "tag-x");
        assert!(
            p.contains("(deny file-write* (subpath \"/Users/alice/.gitmodules\")"),
            ".gitmodules must be write-denied: {p}"
        );
        assert!(
            !p.contains("(deny file-read* file-write* (subpath \"/Users/alice/.gitmodules\")"),
            ".gitmodules must not be read-denied, submodule URLs are not secrets: {p}"
        );
    }

    /// .ripgreprc is denied for write only: rg reads it on startup, and its
    /// contents are rg flags/themes (no secrets), so reads are safe. Write
    /// stays denied to prevent planting malicious rg flags.
    #[test]
    fn test_ripgreprc_write_denied() {
        let p = mandatory_deny("/Users/alice", "tag-x");
        assert!(
            p.contains("(deny file-write* (subpath \"/Users/alice/.ripgreprc\")"),
            ".ripgreprc must be write-denied: {p}"
        );
        assert!(
            !p.contains("(deny file-read* file-write* (subpath \"/Users/alice/.ripgreprc\")"),
            ".ripgreprc must not be read-denied, rg needs to read it and it has no secrets: {p}"
        );
    }
}
