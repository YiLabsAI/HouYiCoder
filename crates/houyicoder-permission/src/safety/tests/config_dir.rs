use super::*;

#[test]
fn test_write_config_dir_asks() {
    let v = serde_json::json!({"path": "/home/u/.claude/settings.json"});
    assert_eq!(
        safety_check("edit", Some(&v)),
        Some(Effect::Ask),
        "a write to another agent's config store must Ask"
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

/// The installed-plugin registries decide which hooks and skills load on the
/// next start, so they stay protected like the stores beside them.
#[test]
fn test_plugin_registry_asks() {
    for command in [
        "echo x > .claude/plugins/installed_plugins.json",
        "rm ~/.claude/plugins/known_marketplaces.json",
    ] {
        let v = serde_json::json!({"command": command});
        assert_eq!(
            safety_check("bash", Some(&v)),
            Some(Effect::Ask),
            "{command}"
        );
    }
}

/// Every control child decides a later permission or a later run, so each one
/// escalates on its own and as the directory above its contents.
#[test]
fn test_control_children_ask() {
    for path in [
        ".claude/settings.local.json",
        ".houyicoder/permissions.local.json",
        ".houyicoder/skill-grants.json",
        ".claude/commands/deploy.md",
        ".agents/agents/reviewer.md",
        ".claude/hooks/pre-commit.sh",
    ] {
        let v = serde_json::json!({ "path": path });
        assert_eq!(safety_check("write", Some(&v)), Some(Effect::Ask), "{path}");
    }
}

/// A backup copy beside a control file is not the control file: nothing reads
/// it to decide a permission, so escalating on it would ask for nothing.
#[test]
fn test_control_copies_no_ask() {
    for path in [
        ".claude/settings.json.bak",
        ".houyicoder/hooks.bak",
        ".claude/plugins.json.old",
    ] {
        let v = serde_json::json!({ "path": path });
        assert_eq!(safety_check("write", Some(&v)), None, "{path}");
    }
}

/// A path written with a redundant step still names the control file, so the
/// collapsed spelling is checked alongside the plain one. A detour through an
/// unrelated child leaves the same file at the end, so it collapses too.
#[test]
fn test_collapsed_separator_asks() {
    for command in [
        "echo x > .claude/./settings.json",
        "rm .houyicoder//permissions.json",
        "echo x > .claude/state/../settings.json",
    ] {
        let v = serde_json::json!({ "command": command });
        assert_eq!(
            safety_check("bash", Some(&v)),
            Some(Effect::Ask),
            "{command}"
        );
    }
}

#[test]
fn test_cross_marker_not_exempt() {
    // A worktree path in one configuration directory does not excuse a write
    // to a store in another: the write lands on a control file either way.
    let v = serde_json::json!({"command": "echo x > .houyicoder/settings.json"});
    assert_eq!(safety_check("bash", Some(&v)), Some(Effect::Ask));
}
