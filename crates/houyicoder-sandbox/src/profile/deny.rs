//! Seatbelt rules for credentials, executable configuration, and files that
//! govern the agent's own permissions. The profile places these rules on either
//! side of the session temporary-directory allow-back, which is what decides
//! whether a fixture settings file under that directory is writable.

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

/// Permission and settings files denied for writing. Changing one would
/// authorize later operations without the user's consent. Match files rather
/// than entire directories so workspaces under those directories remain
/// writable.
///
/// Two lists because the profile emits them on either side of the session
/// scratch allow-back. A permission store can live under the scratch
/// directory, so its deny is emitted last and wins there. A settings file is
/// read only from the config home and the workspace, so its deny is emitted
/// first and the scratch directory stays writable for a fixture project.
const MANDATORY_DENY_PERMISSION_STORES: &[&str] = &[
    ".houyicoder/permissions.json",
    ".houyicoder/permissions.local.json",
    ".houyicoder/skill-grants.json",
    "houyicoder-permissions/permissions.json",
];

/// Settings files, denied before the scratch allow-back. See the split note on
/// MANDATORY_DENY_PERMISSION_STORES. The local overlay is listed with the
/// shared file: it is read at higher precedence than the shared one, so leaving
/// it out would leave the effective setting writable.
const MANDATORY_DENY_SETTINGS_FILES: &[&str] = &[
    ".houyicoder/settings.json",
    ".houyicoder/settings.local.json",
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
    // The authority-file denies are not emitted here: the profile places them
    // on either side of the session scratch allow-back, which is what decides
    // whether a permission store or a fixture settings file under that
    // directory is writable.
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

/// Deny writes to a permission store anywhere on disk. Emitted after the
/// session scratch allow-back, because a local store under that directory is
/// the case the deny exists for.
pub(crate) fn deny_permission_stores(home: &str, tag: &str) -> String {
    let mut out = String::new();
    for file in MANDATORY_DENY_PERMISSION_STORES {
        let escaped = file.replace('.', r"\.");
        let subpath = format!("{home}/{file}");
        out.push_str(&format!(
            "(deny file-write* (subpath \"{subpath}\") (regex #\".*/{escaped}$\") (with message \"{tag}\"))\n"
        ));
    }
    out
}

/// Deny writes to a settings file anywhere on disk except the session scratch
/// directory, which is emitted after this and therefore wins there. Settings are
/// read from the config home and the workspace only, so the scratch directory
/// is free for a fixture project.
pub(crate) fn deny_settings_files(home: &str, tag: &str) -> String {
    let mut out = String::new();
    for file in MANDATORY_DENY_SETTINGS_FILES {
        let escaped = file.replace('.', r"\.");
        let subpath = format!("{home}/{file}");
        out.push_str(&format!(
            "(deny file-write* (subpath \"{subpath}\") (regex #\".*/{escaped}$\") (with message \"{tag}\"))\n"
        ));
    }
    out
}

/// Deny writes to the user-wide snapshot store. The store holds the undo data
/// for destructive commands, so a command that could write it could rewrite its
/// own recovery evidence. Snapshot creation is unaffected: the host process
/// writes the store and is not inside the fence.
pub(crate) fn deny_home_snapshot_store(home: &str, tag: &str) -> String {
    format!(
        "(deny file-write* (subpath \"{home}/.houyicoder/snapshots\") (with message \"{tag}\"))\n"
    )
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
