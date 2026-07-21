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
const PROTECTED: [&str; 6] = [
    ".git/",
    ".houyicoder/",
    ".claude/",
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
/// the worktree-container exemption applied. Split out from safety_check so
/// the same judgement can be applied to a path that has been resolved to the
/// file it actually names, not only to the string the caller supplied.
pub(crate) fn marker_hit(content: &str) -> bool {
    let lower = content.to_ascii_lowercase();
    for marker in PROTECTED {
        if lower.contains(marker) {
            // The worktree container is the workspace, not a config store, so
            // a path there does not trip its config-dir protection. Other
            // markers (.git/ inside a worktree, .bashrc, ...) still fire.
            if is_worktree_subpath(marker, &lower) {
                continue;
            }
            return true;
        }
    }
    false
}

/// Whether the matched marker is followed by a worktree-container subpath, so
/// the call is operating in the agent workspace (not the config store) and the
/// marker is exempt.
fn is_worktree_subpath(marker: &str, lower_content: &str) -> bool {
    WORKTREE_EXEMPTS.iter().any(|exempt| {
        // The exempt must start with the same config-dir prefix (e.g. the
        // .claude/ marker is exempt only by .claude/worktrees/, not by the
        // project's own worktrees/ subpath).
        exempt.starts_with(marker) && lower_content.contains(*exempt)
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
