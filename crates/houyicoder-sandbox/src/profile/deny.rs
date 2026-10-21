//! Seatbelt rules for credentials, executable configuration, and files that
//! govern the agent's own permissions. The authority-file rules are emitted
//! again after the session temporary directory is made writable.

use std::path::Path;

/// Credential and shell-startup files denied for reading and writing.
/// Ordinary source files remain readable across projects, but credentials
/// cannot rely on that broad read policy until value redaction is available.
const MANDATORY_DENY_FILES: &[&str] = &[
    ".bashrc",
    ".bash_profile",
    ".zshrc",
    ".zprofile",
    ".profile",
    ".mcp.json",
    // Workspace .env files remain readable as development inputs.
    ".netrc",
    ".npmrc",
    ".pypirc",
];

/// Git and search configuration denied for writing but kept readable for
/// normal tool operation. Reads of git config can expose embedded credentials;
/// value redaction remains necessary at the transcript boundary.
const MANDATORY_DENY_WRITE_ONLY_FILES: &[&str] =
    &[".gitconfig", ".git/config", ".gitmodules", ".ripgreprc"];

/// Permission and settings files denied for writing wherever they occur.
/// Changing one would authorize later operations without the user's consent.
/// Match files rather than entire directories so workspaces under those
/// directories remain writable.
const MANDATORY_DENY_AUTHORITY_FILES: &[&str] = &[
    ".houyicoder/permissions.json",
    ".houyicoder/settings.json",
    ".houyicoder/skill-grants.json",
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
    ".claude/skills",
    ".agents/skills",
    ".aws",
    ".gnupg",
    ".kube",
    ".docker",
];

/// Skill definition directories denied for write only: the agent must not
/// plant or rewrite a SKILL.md (a directive the model later invokes), but
/// may read the skill's reference and script files — the resource layer.
/// A read-deny adds nothing: the body is host-injected (the deny cannot
/// reach it), and the resource files are the resource layer the deny would
/// block. The write-deny is the load-bearing half. The nested-anywhere
/// regex covers project, user, and managed paths. The ecosystem-compat
/// and agents skill dirs stay read+write denied above (no resource layer).
const MANDATORY_DENY_WRITE_ONLY_DIRS: &[&str] = &[".houyicoder/skills"];

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
    out.push_str(&deny_authority_files(home, tag));
    for dir in MANDATORY_DENY_DIRS {
        let escaped = dir.replace('.', r"\.");
        emit(
            &mut out,
            &format!("{home}/{dir}"),
            &format!(".*/{escaped}(/|$)"),
        );
    }
    // Write-only skill dirs: reads allowed back (the resource layer),
    // writes denied so the agent cannot plant a SKILL.md. Same dir-shaped
    // nested-anywhere regex as the read+write dirs above, with the read
    // verb omitted so the read allow-back further down can take effect.
    for dir in MANDATORY_DENY_WRITE_ONLY_DIRS {
        let escaped = dir.replace('.', r"\.");
        out.push_str(&format!(
            "(deny file-write* (subpath \"{home}/{dir}\") (regex #\".*/{escaped}(/|$)\") (with message \"{tag}\"))\n"
        ));
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

/// Emit the write-deny rules for the self-authorization files. Re-emitted
/// after the session temporary-directory allow-back so the local permission
/// store under the temporary directory stays write-denied even while ordinary
/// files and repositories inside that directory are writable.
pub(crate) fn deny_authority_files(home: &str, tag: &str) -> String {
    let mut out = String::new();
    for file in MANDATORY_DENY_AUTHORITY_FILES {
        let escaped = file.replace('.', r"\.");
        let subpath = format!("{home}/{file}");
        out.push_str(&format!(
            "(deny file-write* (subpath \"{subpath}\") (regex #\".*/{escaped}$\") (with message \"{tag}\"))\n"
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
mod tests;
