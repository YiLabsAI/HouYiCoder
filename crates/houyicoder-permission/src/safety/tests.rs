mod claude_dir;

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
