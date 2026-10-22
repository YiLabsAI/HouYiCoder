//! Seatbelt rules for credentials, executable configuration, and the files that
//! decide what the agent is allowed to do.

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

/// Permission stores, denied for writing anywhere. Writing one authorizes later
/// operations without the user's consent.
///
/// The profile emits this deny after the rule that makes the session scratch
/// directory writable, so it wins there: a local store can live in that
/// directory and must stay closed.
const MANDATORY_DENY_PERMISSION_STORES: &[&str] = &[
    ".houyicoder/permissions.json",
    ".houyicoder/permissions.local.json",
    ".houyicoder/skill-grants.json",
    "houyicoder-permissions/permissions.json",
];

/// Settings files, denied for writing everywhere except the session scratch
/// directory, which is emitted after this and therefore stays writable: a
/// sandboxed test lays out its own project there, and a settings file is only
/// ever read from the config home and the workspace.
///
/// Each local overlay sits beside the file it overrides. It is read at higher
/// precedence, so denying only the shared file would leave the effective
/// setting writable.
const MANDATORY_DENY_SETTINGS_FILES: &[&str] = &[
    ".houyicoder/settings.json",
    ".houyicoder/settings.local.json",
];

/// Home directories denied for reading and writing. They hold credentials
/// (.ssh, .aws, .gnupg, .kube, .docker) or the agent and skill definitions that
/// decide what runs later, so reading them leaks secrets and writing them
/// hijacks a future session. Denied outright until value redaction exists.
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

/// Git hook directory, denied for writing only. A planted hook runs the next
/// time the user checks out, so the write is what has to be blocked; a hook may
/// also hold a secret, which is the same interim read exposure as the config.
const MANDATORY_DENY_GIT_PATHS: &[&str] = &[".git/hooks"];

/// Deny the credential and instruction paths above, for reading and writing.
///
/// Each entry is denied twice: at its home path, and as a trailing path segment
/// anywhere on disk. The second form is what makes the deny hold inside a
/// workspace that was opened for writing. Emitted after the filesystem rules so
/// an allow-back cannot wash it out.
///
/// The regex uses the ICU syntax sandbox-exec accepts: a file name matches as
/// the last segment, a directory as the last segment plus a separator or the end
/// of the path.
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
    out.push_str(&deny_write_files(
        MANDATORY_DENY_WRITE_ONLY_FILES,
        home,
        tag,
    ));
    // The permission and settings denies are not emitted here: the profile
    // places them on either side of the scratch-directory allow rule.
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

fn deny_write_files(files: &[&str], home: &str, tag: &str) -> String {
    let mut out = String::new();
    for file in files {
        let escaped = file.replace('.', r"\.");
        let subpath = format!("{home}/{file}");
        out.push_str(&format!(
            "(deny file-write* (subpath \"{subpath}\") (regex #\".*/{escaped}$\") (with message \"{tag}\"))\n"
        ));
    }
    out
}

/// Deny writes to a permission store anywhere on disk. Emitted after the
/// session scratch allow-back, because a local store under that directory is
/// the case the deny exists for.
pub(crate) fn deny_permission_stores(home: &str, tag: &str) -> String {
    deny_write_files(MANDATORY_DENY_PERMISSION_STORES, home, tag)
}

/// Deny writes to a settings file anywhere on disk except the session scratch
/// directory, which is emitted after this and therefore wins there. Settings are
/// read from the config home and the workspace only, so the scratch directory
/// is free for a fixture project.
pub(crate) fn deny_settings_files(home: &str, tag: &str) -> String {
    deny_write_files(MANDATORY_DENY_SETTINGS_FILES, home, tag)
}

/// Subtrees of the shared temp root that belong to other processes: launchd and
/// application sockets, tmux sessions, the display and font servers, ssh agent
/// sockets, editor IPC endpoints, and postgres sockets. Writing there can break
/// or hijack a process outside the session, so the shared root is writable and
/// these prefixes are not.
const GUARDED_SHARED_TEMP: &[&str] = &[
    "com\\.apple\\.",
    "tmux-",
    "\\.X11-unix",
    "\\.ICE-unix",
    "\\.XIM-unix",
    "\\.font-unix",
    "ssh-",
    "\\.vscode-ipc-",
    "\\.s\\.PGSQL\\.",
];

/// Deny writes to the guarded subtrees of the shared temp root, in both the
/// /tmp spelling and the /private/tmp one the kernel resolves to.
pub(crate) fn deny_shared_temp_guarded(tag: &str) -> String {
    let alternation = GUARDED_SHARED_TEMP.join("|");
    let mut out = String::new();
    for root in ["/tmp", "/private/tmp"] {
        out.push_str(&format!(
            "(deny file-write* (regex #\"^{root}/({alternation})\") (with message \"{tag}\"))\n"
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
