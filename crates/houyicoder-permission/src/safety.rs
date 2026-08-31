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
        crate::heredoc::strip_quoted_heredoc_bodies(&content)
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
mod tests {
    use super::*;

    #[test]
    fn test_bash_touching_git_dir() {
        let v = serde_json::json!({"command": "rm -rf .git/"});
        assert_eq!(safety_check("bash", Some(&v)), Some(Effect::Ask));
    }

    /// All three skill-directory families are bypass-immune: a command that
    /// runs or touches a skill under any of them must Ask in every mode,
    /// regardless of any stored always-allow rule. The safety stage fires
    /// before the rule-allow stage, so an untrusted skill script cannot be
    /// pre-authorized by a broad rule. This is the load-bearing structure
    /// for the skill-script execution MUST — without the third family
    /// here, a script from the spec-interop source could be let through by
    /// a blanket bash always-allow rule.
    #[test]
    fn test_skill_dirs_bypass_immune() {
        for dir in [".houyicoder/skills/", ".claude/skills/", ".agents/skills/"] {
            let v = serde_json::json!({"command": format!("bash {dir}evil/run.sh")});
            assert_eq!(
                safety_check("bash", Some(&v)),
                Some(Effect::Ask),
                "{dir} must escalate to Ask (bypass-immune)"
            );
        }
        // A nested config dir INSIDE a worktree container must still fire: the
        // per-occurrence exempt skips only the container occurrence, not the
        // nested marker. Without this a workspace skill script under a blanket
        // bash allow would execute with no second confirmation.
        for cmd in [
            "bash /srv/repo/.houyicoder/worktrees/feat/.houyicoder/skills/evil/run.sh",
            "bash /srv/repo/.claude/worktrees/s6t2-gate/.claude/skills/evil/run.sh",
        ] {
            let v = serde_json::json!({"command": cmd});
            assert_eq!(
                safety_check("bash", Some(&v)),
                Some(Effect::Ask),
                "nested config dir in worktree must still Ask: {cmd}"
            );
        }
    }

    /// The agent's own config dir is bypass-immune: a write to its settings,
    /// permissions, or memory must Ask in every mode. Rewriting the file that
    /// governs what the agent may do is how one approval would widen every
    /// later one, so it is never silently allowed.
    #[test]
    fn test_edit_touching_config_dir() {
        let v = serde_json::json!({"path": "/home/u/.houyicoder/settings.json"});
        assert_eq!(
            safety_check("edit", Some(&v)),
            Some(Effect::Ask),
            "a write to the project config dir must escalate to Ask"
        );
    }

    #[test]
    fn test_bash_write_config_asks() {
        let v = serde_json::json!({"command": "echo x > .houyicoder/permissions.json"});
        assert_eq!(safety_check("bash", Some(&v)), Some(Effect::Ask));
    }

    #[test]
    fn test_cd_worktree_no_ask() {
        let v = serde_json::json!({"command": "cd .houyicoder/worktrees/feature-x"});
        assert_eq!(safety_check("bash", Some(&v)), None);
    }

    #[test]
    fn test_edit_worktree_passes() {
        let v = serde_json::json!({"path": ".houyicoder/worktrees/feature-x/src/main.rs"});
        assert_eq!(safety_check("edit", Some(&v)), None);
    }

    #[test]
    fn test_git_in_worktree_asks() {
        let v = serde_json::json!({"command": "rm -rf .houyicoder/worktrees/x/.git/"});
        assert_eq!(safety_check("bash", Some(&v)), Some(Effect::Ask));
    }

    #[test]
    fn test_bash_touching_bashrc_asks() {
        let v = serde_json::json!({"command": "echo x >> ~/.bashrc"});
        assert_eq!(safety_check("bash", Some(&v)), Some(Effect::Ask));
    }

    #[test]
    fn test_bash_touching_zshrc_asks() {
        let v = serde_json::json!({"command": "cat >> ~/.zshrc"});
        assert_eq!(safety_check("bash", Some(&v)), Some(Effect::Ask));
    }

    #[test]
    fn test_bash_touching_profile_asks() {
        let v = serde_json::json!({"command": "echo y >> ~/.profile"});
        assert_eq!(safety_check("bash", Some(&v)), Some(Effect::Ask));
    }

    #[test]
    fn test_safe_command_no_match() {
        let v = serde_json::json!({"command": "ls -la /tmp"});
        assert_eq!(safety_check("bash", Some(&v)), None);
    }

    #[test]
    fn test_safe_path_no_match() {
        let v = serde_json::json!({"path": "/tmp/notes.txt"});
        assert_eq!(safety_check("edit", Some(&v)), None);
    }

    #[test]
    fn test_empty_input_no_match() {
        assert_eq!(safety_check("bash", None), None);
    }

    #[test]
    fn test_case_insensitive_marker() {
        // .GIT/ in the command still triggers (case-insensitive).
        let v = serde_json::json!({"command": "rm -rf .GIT/"});
        assert_eq!(safety_check("bash", Some(&v)), Some(Effect::Ask));
    }

    #[test]
    fn test_nested_git_path_triggers() {
        let v = serde_json::json!({"path": "repo/.git/hooks/pre-commit"});
        assert_eq!(safety_check("write", Some(&v)), Some(Effect::Ask));
    }

    /// A Windows-style path with backslashes still matches the forward-slash
    /// markers after normalization. Without it the gate would silently Allow
    /// a protected path on Windows.
    #[test]
    fn test_backslash_path_matches_marker() {
        let v = serde_json::json!({"command": "cat repo\\.git\\config"});
        assert_eq!(safety_check("bash", Some(&v)), Some(Effect::Ask));
    }
}

#[cfg(test)]
mod claude_dir_tests {
    use super::*;

    #[test]
    fn test_write_config_dir_asks() {
        let v = serde_json::json!({"path": "/home/u/.claude/settings.json"});
        assert_eq!(
            safety_check("edit", Some(&v)),
            Some(Effect::Ask),
            "a write to the CC config dir must Ask (agent must not silently rewrite another agent's config)"
        );
    }

    #[test]
    fn test_cd_worktree_no_ask() {
        let v = serde_json::json!({"command": "cd .claude/worktrees/feature-x"});
        assert_eq!(safety_check("bash", Some(&v)), None);
    }

    #[test]
    fn test_worktree_git_still_asks() {
        let v = serde_json::json!({"command": "rm -rf .claude/worktrees/x/.git/"});
        assert_eq!(safety_check("bash", Some(&v)), Some(Effect::Ask));
    }

    #[test]
    fn test_cross_marker_not_exempt() {
        // The project's own config-dir marker must not be exempted by
        // .claude/worktrees/ (and vice versa) — the exemption is per-marker.
        let v = serde_json::json!({"command": "echo x > .houyicoder/settings.json"});
        assert_eq!(safety_check("bash", Some(&v)), Some(Effect::Ask));
    }
}
