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
