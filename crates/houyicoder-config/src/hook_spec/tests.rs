//! Tests for this module.

use super::*;

#[test]
fn test_parse_hooks_empty() {
    assert!(parse_hooks(None).unwrap().is_empty());
    assert!(parse_hooks(Some("")).unwrap().is_empty());
    assert!(parse_hooks(Some("   ")).unwrap().is_empty());
}

#[test]
fn test_parse_hooks_array() {
    let raw = r#"[{"name":"lint","events":["PreToolUse"],"program":"sh","args":["-c","echo hi"]}]"#;
    let list = parse_hooks(Some(raw)).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name, "lint");
    assert_eq!(list[0].events, vec!["PreToolUse"]);
    assert_eq!(list[0].program, "sh");
    assert_eq!(list[0].args, vec!["-c", "echo hi"]);
}

#[test]
fn test_parse_hooks_invalid() {
    assert!(parse_hooks(Some("not json")).is_err());
    assert!(parse_hooks(Some(r#"[{"events":[],"program":"sh"}]"#)).is_err());
}

#[test]
fn test_parse_hooks_round_trips() {
    let spec = HookSpec {
        name: "lint".into(),
        events: vec!["PreToolUse".into()],
        program: "sh".into(),
        args: vec!["-c".into(), "echo hi".into()],
        matcher: None,
        if_condition: None,
        shell: None,
        timeout_secs: None,
        once: false,
    };
    let s = serde_json::to_string(&spec).unwrap();
    let back: HookSpec = serde_json::from_str(&s).unwrap();
    assert_eq!(spec, back);
}

#[test]
fn test_parse_hooks_drops_empty() {
    let raw = r#"[{"name":"","events":["PreToolUse"],"program":"sh"},{"name":"x","events":[],"program":""}]"#;
    let list = parse_hooks(Some(raw)).unwrap();
    assert!(list.is_empty());
}

#[test]
fn test_parse_settings_hooks_basic() {
    let val = serde_json::json!({
        "hooks": {
            "PreToolUse": [
                {
                    "matcher": "Bash|Edit",
                    "hooks": [
                        {"type": "command", "command": "echo lint", "timeout": 30, "once": true}
                    ]
                }
            ],
            "PostToolUse": [
                {
                    "hooks": [
                        {"type": "command", "command": "echo done", "if": "Bash(git *)"}
                    ]
                }
            ]
        }
    });
    let specs = parse_hooks_from_settings(&val);
    assert_eq!(specs.len(), 2);
    let pre = specs.iter().find(|s| s.events[0] == "PreToolUse").unwrap();
    assert_eq!(pre.matcher.as_deref(), Some("Bash|Edit"));
    assert_eq!(pre.timeout_secs, Some(30));
    assert!(pre.once, "once field parsed from settings");
    assert_eq!(pre.program, "sh");
    assert_eq!(pre.args, vec!["-c", "echo lint"]);
    // bash shell stays bash (not mapped to sh).
    let bash_val = serde_json::json!({
        "hooks": {
            "PreToolUse": [
                {"hooks": [{"type": "command", "command": "echo hi", "shell": "bash"}]}
            ]
        }
    });
    let bash_specs = parse_hooks_from_settings(&bash_val);
    assert_eq!(bash_specs[0].program, "bash");
    let post = specs.iter().find(|s| s.events[0] == "PostToolUse").unwrap();
    assert_eq!(post.if_condition.as_deref(), Some("Bash(git *)"));
    assert!(post.matcher.is_none());
}

#[test]
fn test_parse_settings_hooks_noncmd() {
    let val = serde_json::json!({
        "hooks": {
            "PreToolUse": [
                {
                    "hooks": [
                        {"type": "prompt", "prompt": "check this"},
                        {"type": "command", "command": "echo ok"}
                    ]
                }
            ]
        }
    });
    let specs = parse_hooks_from_settings(&val);
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].args, vec!["-c", "echo ok"]);
}

#[test]
fn test_parse_settings_hooks_empty() {
    let specs = parse_hooks_from_settings(&serde_json::json!({}));
    assert!(specs.is_empty());
    let specs = parse_hooks_from_settings(&serde_json::json!({"hooks": {}}));
    assert!(specs.is_empty());
}

#[test]
fn test_resolve_hook_policy_defaults() {
    let p = resolve_hook_policy_settings(&serde_json::json!({}));
    assert!(!p.disable_all_hooks);
    assert!(!p.allow_managed_hooks_only);
}

#[test]
fn test_hook_policy_disable_all() {
    let p = resolve_hook_policy_settings(&serde_json::json!({"disableAllHooks": true}));
    assert!(p.disable_all_hooks);
    assert!(!p.allow_managed_hooks_only);
}

#[test]
fn test_hook_policy_managed_only() {
    let p = resolve_hook_policy_settings(&serde_json::json!({"allowManagedHooksOnly": true}));
    assert!(!p.disable_all_hooks);
    assert!(p.allow_managed_hooks_only);
}
