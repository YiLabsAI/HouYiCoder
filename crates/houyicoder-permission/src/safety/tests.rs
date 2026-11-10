mod config_dir;

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

/// Session logs and captured output live under the configuration directory
/// but decide nothing. Escalating on them turns every read of the agent's own
/// records into a bypass-immune ask the user cannot silence.
#[test]
fn test_session_data_no_ask() {
    for path in [
        "/home/u/.houyicoder/sessions/2026-10-29.jsonl",
        ".houyicoder/bash-output/01HXY.txt",
        ".claude/projects/-home-u-repo/transcript.jsonl",
    ] {
        let v = serde_json::json!({ "path": path });
        assert_eq!(safety_check("read", Some(&v)), None, "{path}");
    }
}

/// A file whose name merely ends in a protected name is not that file.
#[test]
fn test_similar_name_no_ask() {
    for command in ["cat ./install.bashrc", "cat AGENTS.md.bak"] {
        let v = serde_json::json!({ "command": command });
        assert_eq!(safety_check("bash", Some(&v)), None, "{command}");
    }
}

/// Rewriting an instruction file changes every later decision, so a shell
/// command that writes one escalates even where a plain file write inside
/// the workspace would not.
#[test]
fn test_instruction_file_asks() {
    for command in [
        "echo x > AGENTS.md",
        "printf y >> /home/u/repo/CLAUDE.md",
        "cat notes >> SKILL.md",
    ] {
        let v = serde_json::json!({ "command": command });
        assert_eq!(
            safety_check("bash", Some(&v)),
            Some(Effect::Ask),
            "{command}"
        );
    }
}

/// Reading an instruction file decides nothing, so the read does not turn
/// into a bypass-immune ask the user cannot silence.
#[test]
fn test_instruction_read_no_ask() {
    for command in ["cat AGENTS.md", "rg hooks /home/u/repo/CLAUDE.md"] {
        let v = serde_json::json!({ "command": command });
        assert_eq!(safety_check("bash", Some(&v)), None, "{command}");
    }
}

/// A write tool naming an instruction file escalates: no shell read-only
/// waiver applies, because the call does not run a command at all.
#[test]
fn test_edit_instruction_file_asks() {
    for path in ["AGENTS.md", "/home/u/repo/CLAUDE.md", "docs/SKILL.md"] {
        let v = serde_json::json!({ "path": path });
        assert_eq!(safety_check("write", Some(&v)), Some(Effect::Ask), "{path}");
    }
}

/// The memory store is recalled into a later prompt, so a write to it plants
/// an instruction the model follows then. Both layouts the store uses are
/// protected, and a directory that is merely named memory is not.
#[test]
fn test_memory_store_asks() {
    for path in [
        "/home/u/.houyicoder/memory/MEMORY.md",
        "/home/u/.houyicoder/projects/-home-u-repo/memory/MEMORY.md",
        ".houyicoder/memory/notes/topic.md",
    ] {
        let v = serde_json::json!({ "path": path });
        assert_eq!(safety_check("write", Some(&v)), Some(Effect::Ask), "{path}");
    }
    for path in [
        "/tmp/memory/notes.md",
        "/home/u/.claude/worktrees/w1/src/memory/store.rs",
    ] {
        let v = serde_json::json!({ "path": path });
        assert_eq!(safety_check("write", Some(&v)), None, "{path}");
    }
}

/// The store is loaded before the first turn, so a read-only command that
/// reads one adds no exposure. A command that writes it still escalates.
#[test]
fn test_memory_read_no_ask() {
    for command in [
        "cat ~/.houyicoder/memory/MEMORY.md",
        "head -20 .houyicoder/memory/notes/topic.md",
    ] {
        let v = serde_json::json!({ "command": command });
        assert_eq!(safety_check("bash", Some(&v)), None, "{command}");
    }
    for command in [
        "echo x >> ~/.houyicoder/memory/MEMORY.md",
        "cp notes.md .houyicoder/memory/",
    ] {
        let v = serde_json::json!({ "command": command });
        assert_eq!(
            safety_check("bash", Some(&v)),
            Some(Effect::Ask),
            "{command}"
        );
    }
}

/// The snapshot store is what a restore reads, so rewriting it rewrites the
/// state a later turn may go back to.
#[test]
fn test_snapshot_store_asks() {
    for path in [
        "/home/u/.houyicoder/snapshots/s-01.json",
        ".claude/snapshots/index.json",
    ] {
        let v = serde_json::json!({ "path": path });
        assert_eq!(safety_check("write", Some(&v)), Some(Effect::Ask), "{path}");
    }
}

/// The stored credentials decide which account the agent acts as, so even a
/// read of them escalates.
#[test]
fn test_credentials_file_asks() {
    let v = serde_json::json!({ "path": "/home/u/.claude/.credentials.json" });
    assert_eq!(safety_check("read", Some(&v)), Some(Effect::Ask));
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

/// A startup file can export secrets, so reading one still escalates even
/// though a verified read of an instruction file does not.
#[test]
fn test_startup_read_asks() {
    let v = serde_json::json!({"command": "cat ~/.bashrc"});
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
