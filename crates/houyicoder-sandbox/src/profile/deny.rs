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
