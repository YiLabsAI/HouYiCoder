//! Bypass-immune safety checks. Certain paths are protected so strongly that
//! even Bypass mode (and a stored consent) cannot auto-allow a write to them:
//! the version-control directory, the agent settings directory, and the shell
//! rc files. A call that touches one of these always escalates to Ask so the
//! user inspects it, regardless of the current mode or any allow-rule.
//!
//! The check is a coarse substring heuristic over the call's primary content
//! string (command for shell, path for file tools). It errs on the side of
//! Ask: a false positive asks the user (safe), a false negative is caught by
//! the sandbox fence. The goal is to surface the high-risk writes the
//! autonomous mode must never silently perform.

use serde_json::Value;

use crate::heredoc::strip_quoted_heredoc_bodies;
use crate::rule::{Effect, input_content};

/// Markers for protected paths. A call whose primary content string contains
/// any of these (case-insensitive) escalates to Ask. The trailing slash on the
/// directory markers avoids matching bare names while still catching nested
/// paths like repo/.git/hooks.
const PROTECTED: [&str; 7] = [
    ".git/",
    ".houyicoder/",
    ".claude/",
    ".agents/",
    ".bashrc",
    ".zshrc",
    ".profile",
];

/// Worktree container subpaths exempt from the config-dir markers. These are
/// the agent's workspace, not a config store, so cd/read/write there is normal
/// work — the config-dir markers guard settings/permissions/memory, not the
/// worktree workspace.
const WORKTREE_EXEMPTS: &[&str] = &[".houyicoder/worktrees/", ".claude/worktrees/"];

/// Whether a tool call touches a protected path. Returns Ask when the call's
/// primary content string references one of the bypass-immune markers; None
/// otherwise. The check is bypass-immune: it fires before the mode default, so
/// even Bypass mode cannot silence it. An exception: a path under a worktree
/// container is exempt from its config-dir marker (it is the workspace, not
/// the config store) — other markers (.git/, .bashrc, ...) still fire.
pub fn safety_check(tool_name: &str, input: Option<&Value>) -> Option<Effect> {
    let content = input_content(tool_name, input);
    if content.is_empty() {
        return None;
    }
    // Strip quoted heredoc bodies: a cat <<'EOF' body is literal text, so a
    // protected path (.git/, .bashrc, ...) appearing in it must not trip
    // this bypass-immune check. Unquoted heredoc bodies stay (bash expands
    // them).
    let content = if matches!(
        tool_name.to_ascii_lowercase().as_str(),
        "bash" | "sh" | "exec" | "shell"
    ) {
        strip_quoted_heredoc_bodies(&content)
    } else {
        content
    };
    if marker_hit(&content) {
        return Some(Effect::Ask);
    }
    None
}

/// Whether a string references a protected marker, case-insensitively, with
/// the worktree-container exemption applied PER OCCURRENCE. Split out from
/// safety_check so the same judgement can be applied to a path that has been
/// resolved to the file it actually names, not only to the string the caller
/// supplied.
pub(crate) fn marker_hit(content: &str) -> bool {
    // Normalize backslashes to forward slashes so the forward-slash markers
    // match Windows paths. No-op on Unix.
    let lower = content.replace('\\', "/").to_ascii_lowercase();
    for marker in PROTECTED {
        // For each occurrence of the marker, check whether that occurrence is
        // a worktree container path (the workspace, exempt) or a real config
        // dir (fire). The exempt is per-occurrence, not whole-content: a
        // nested config dir inside the workspace (a second marker occurrence
        // after the container) must still fire, or a workspace skill script
        // would bypass the protected-path ask under a blanket bash allow.
        for idx in lower.match_indices(marker).map(|(i, _)| i) {
            let after = &lower[idx + marker.len()..];
            if !is_container_occurrence(marker, after) {
                return true;
            }
        }
    }
    false
}

/// Whether a marker occurrence is immediately followed by a worktree-container
/// suffix, meaning the occurrence names the workspace, not a config store.
/// Only the occurrence that IS the container path is exempt; a nested
/// config-dir occurrence (not followed by the container suffix) still fires.
fn is_container_occurrence(marker: &str, after_marker: &str) -> bool {
    WORKTREE_EXEMPTS.iter().any(|exempt| {
        exempt.starts_with(marker) && after_marker.starts_with(&exempt[marker.len()..])
    })
}

#[cfg(test)]
mod tests;
