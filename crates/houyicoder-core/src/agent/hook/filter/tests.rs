//! Tests for this module.

use super::*;
use houyicoder_context::SessionId;

fn ctx_pre(tool: &str) -> HookContext {
    HookContext {
        event: super::super::HookEvent::PreToolUse,
        payload: HookPayload::PreToolUse {
            tool_name: tool.into(),
            input: serde_json::json!({"command": "git push"}),
            backfilled_input: None,
        },
        session: SessionId::new(),
    }
}

#[test]
fn test_matcher_empty_matches_all() {
    assert!(matcher_passes(&ctx_pre("Bash"), ""));
}

#[test]
fn test_matcher_star_matches_all() {
    assert!(matcher_passes(&ctx_pre("Bash"), "*"));
}

#[test]
fn test_matcher_exact() {
    assert!(matcher_passes(&ctx_pre("Bash"), "Bash"));
    assert!(!matcher_passes(&ctx_pre("Edit"), "Bash"));
}

#[test]
fn test_matcher_case_insensitive() {
    // Tool names are lowercase in the engine; user config uses
    // capitalized names. The matcher compares case-insensitively.
    assert!(matcher_passes(&ctx_pre("bash"), "Bash"));
    assert!(matcher_passes(&ctx_pre("Bash"), "bash"));
    assert!(matcher_passes(&ctx_pre("bash"), "BASH"));
}

#[test]
fn test_matcher_regex_case_insensitive() {
    // The regex branch must be case-insensitive too, or a config
    // written against the capitalized ecosystem names silently
    // stops matching the moment the user switches from an exact
    // matcher to an anchored alternation.
    assert!(matcher_passes(&ctx_pre("bash"), "^(Bash|Edit)$"));
    assert!(matcher_passes(&ctx_pre("edit"), "^(Bash|Edit)$"));
    assert!(!matcher_passes(&ctx_pre("read"), "^(Bash|Edit)$"));
    let re = compile_matcher("^(Bash|Edit)$").expect("regex matcher compiles");
    assert!(matcher_passes_compiled(
        &ctx_pre("bash"),
        "^(Bash|Edit)$",
        Some(&re)
    ));
}

#[test]
fn test_matcher_pipe() {
    assert!(matcher_passes(&ctx_pre("Bash"), "Bash|Edit"));
    assert!(matcher_passes(&ctx_pre("Edit"), "Bash|Edit"));
    assert!(!matcher_passes(&ctx_pre("Read"), "Bash|Edit"));
}

#[test]
fn test_matcher_regex() {
    assert!(matcher_passes(&ctx_pre("Bash"), "^(Bash|Edit)$"));
    assert!(matcher_passes(&ctx_pre("Edit"), "^(Bash|Edit)$"));
    assert!(!matcher_passes(&ctx_pre("Read"), "^(Bash|Edit)$"));
}

#[test]
fn test_if_rule_tool_only() {
    assert!(if_rule_passes(&ctx_pre("Bash"), "Bash"));
    assert!(!if_rule_passes(&ctx_pre("Edit"), "Bash"));
}

#[test]
fn test_if_rule_case_insensitive() {
    assert!(if_rule_passes(&ctx_pre("bash"), "Bash"));
    assert!(if_rule_passes(&ctx_pre("Bash"), "bash"));
}

#[test]
fn test_if_rule_with_pattern() {
    assert!(if_rule_passes(&ctx_pre("Bash"), "Bash(git *)"));
    assert!(!if_rule_passes(&ctx_pre("Bash"), "Bash(npm *)"));
}

#[test]
fn test_if_rule_content_bash() {
    // Bash matches input.command, not other string fields.
    let ctx = HookContext {
        event: super::super::HookEvent::PreToolUse,
        payload: HookPayload::PreToolUse {
            tool_name: "Bash".into(),
            input: serde_json::json!({"command": "git push", "cwd": "/tmp"}),
            backfilled_input: None,
        },
        session: SessionId::new(),
    };
    assert!(if_rule_passes(&ctx, "Bash(git *)"));
    assert!(!if_rule_passes(&ctx, "Bash(/tmp*)"));
}

#[test]
fn test_if_rule_content_write() {
    // Write matches input.path.
    let ctx = HookContext {
        event: super::super::HookEvent::PreToolUse,
        payload: HookPayload::PreToolUse {
            tool_name: "Write".into(),
            input: serde_json::json!({"path": "/etc/passwd", "content": "root"}),
            backfilled_input: None,
        },
        session: SessionId::new(),
    };
    assert!(if_rule_passes(&ctx, "Write(/etc/*)"));
    assert!(!if_rule_passes(&ctx, "Write(root*)"));
}

#[test]
fn test_if_unknown_walks_strings() {
    // Unknown tool falls back to walking all strings.
    let ctx = HookContext {
        event: super::super::HookEvent::PreToolUse,
        payload: HookPayload::PreToolUse {
            tool_name: "CustomTool".into(),
            input: serde_json::json!({"data": "hello world"}),
            backfilled_input: None,
        },
        session: SessionId::new(),
    };
    assert!(if_rule_passes(&ctx, "CustomTool(hello*)"));
}

#[test]
fn test_nontool_event_no_match() {
    let ctx = HookContext {
        event: super::super::HookEvent::SessionStart,
        payload: HookPayload::SessionStart { resumed: false },
        session: SessionId::new(),
    };
    assert!(!matcher_passes(&ctx, "Bash"));
}

#[test]
fn test_compile_matcher_exact() {
    assert!(compile_matcher("Bash").is_none());
    assert!(compile_matcher("Bash|Edit").is_none());
    assert!(compile_matcher("").is_none());
    assert!(compile_matcher("*").is_none());
}

#[test]
fn test_compile_matcher_regex() {
    assert!(compile_matcher("^(Bash|Edit)$").is_some());
}

#[test]
fn test_compile_matcher_bad_regex() {
    assert!(compile_matcher("[invalid").is_none());
}

#[test]
fn test_matcher_compiled_exact() {
    let ctx = ctx_pre("Bash");
    assert!(matcher_passes_compiled(&ctx, "Bash", None));
    assert!(!matcher_passes_compiled(&ctx, "Edit", None));
}

#[test]
fn test_matcher_compiled_regex() {
    let re = compile_matcher("^(Bash|Edit)$").unwrap();
    assert!(matcher_passes_compiled(
        &ctx_pre("Bash"),
        "^(Bash|Edit)$",
        Some(&re)
    ));
    assert!(!matcher_passes_compiled(
        &ctx_pre("Read"),
        "^(Bash|Edit)$",
        Some(&re)
    ));
}

#[test]
fn test_compile_if_bare() {
    assert!(compile_if_pattern("Bash").is_none());
}

#[test]
fn test_compile_if_glob() {
    assert!(compile_if_pattern("Bash(git *)").is_some());
}

#[test]
fn test_if_rule_compiled_bash() {
    let re = compile_if_pattern("Bash(git *)").unwrap();
    let ctx = HookContext {
        event: super::super::HookEvent::PreToolUse,
        payload: HookPayload::PreToolUse {
            tool_name: "Bash".into(),
            input: serde_json::json!({"command": "git push"}),
            backfilled_input: None,
        },
        session: SessionId::new(),
    };
    assert!(if_rule_passes_compiled(&ctx, "Bash(git *)", Some(&re)));
}

#[test]
fn test_match_query_session_start() {
    let ctx = HookContext {
        event: super::super::HookEvent::SessionStart,
        payload: HookPayload::SessionStart { resumed: false },
        session: SessionId::new(),
    };
    assert_eq!(match_query(&ctx).as_deref(), Some("startup"));
    assert!(matcher_passes(&ctx, "startup"));
    assert!(!matcher_passes(&ctx, "resume"));
}

#[test]
fn test_match_query_session_resume() {
    let ctx = HookContext {
        event: super::super::HookEvent::SessionStart,
        payload: HookPayload::SessionStart { resumed: true },
        session: SessionId::new(),
    };
    assert_eq!(match_query(&ctx).as_deref(), Some("resume"));
    assert!(matcher_passes(&ctx, "resume"));
}

#[test]
fn test_match_query_setup() {
    let ctx = HookContext {
        event: super::super::HookEvent::Setup,
        payload: HookPayload::Setup,
        session: SessionId::new(),
    };
    assert_eq!(match_query(&ctx).as_deref(), Some("setup"));
    assert!(matcher_passes(&ctx, "setup"));
}

#[test]
fn test_match_query_file_changed() {
    let ctx = HookContext {
        event: super::super::HookEvent::FileChanged,
        payload: HookPayload::FileChanged {
            paths: vec!["/tmp/foo.txt".into()],
        },
        session: SessionId::new(),
    };
    assert_eq!(match_query(&ctx).as_deref(), Some("foo.txt"));
    assert!(matcher_passes(&ctx, "foo.txt"));
    assert!(matcher_passes(&ctx, r".*\.txt"));
}

#[test]
fn test_match_query_subagent() {
    // A matcher selects which subagent kind the hook fires for.
    let ctx = HookContext {
        event: super::super::HookEvent::SubagentStop,
        payload: HookPayload::SubagentStop {
            agent_id: houyicoder_context::AgentId("a1".into()),
            agent_type: "code-reviewer".into(),
            status: "completed".into(),
            last_text: None,
        },
        session: SessionId::new(),
    };
    assert_eq!(match_query(&ctx).as_deref(), Some("code-reviewer"));
    assert!(matcher_passes(&ctx, "code-reviewer"));
    assert!(!matcher_passes(&ctx, "planner"));
}

#[test]
fn test_match_query_session_end() {
    let ctx = HookContext {
        event: super::super::HookEvent::SessionEnd,
        payload: HookPayload::SessionEnd {
            reason: SessionEndReason::Logout,
        },
        session: SessionId::new(),
    };
    assert_eq!(match_query(&ctx).as_deref(), Some("logout"));
    assert!(matcher_passes(&ctx, "logout"));
    assert!(!matcher_passes(&ctx, "clear"));
}

#[test]
fn test_match_query_compact_trigger() {
    // The trigger string comes from the enum's own accessor, not a
    // debug rendering, so renaming the variant cannot silently
    // change what a configured matcher selects.
    let ctx = HookContext {
        event: super::super::HookEvent::PreCompact,
        payload: HookPayload::PreCompact {
            trigger: super::super::CompactTrigger::Auto,
            pre_compact_event_count: 0,
            pre_compact_token_estimate: 0,
        },
        session: SessionId::new(),
    };
    assert_eq!(match_query(&ctx).as_deref(), Some("auto"));
    assert!(matcher_passes(&ctx, "auto"));
    assert!(!matcher_passes(&ctx, "manual"));
}

#[test]
fn test_match_query_unmapped_event() {
    // An event with no meaningful kind field matches only the
    // wildcard forms, never a named matcher.
    let ctx = HookContext {
        event: super::super::HookEvent::PreSelect,
        payload: HookPayload::PreSelect {
            current_token_estimate: 0,
        },
        session: SessionId::new(),
    };
    assert_eq!(match_query(&ctx), None);
    assert!(matcher_passes(&ctx, ""));
    assert!(matcher_passes(&ctx, "*"));
    assert!(!matcher_passes(&ctx, "anything"));
}
