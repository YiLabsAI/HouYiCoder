use super::*;
use std::slice::from_ref;

#[test]
fn test_eval_last_match_wins() {
    let rules = vec![
        Rule::new("bash", Effect::Ask).unwrap(),
        Rule::new("bash", Effect::Allow).unwrap(),
    ];
    assert_eq!(evaluate(&rules, "bash", "ls"), Some(Effect::Allow));
}

#[test]
fn test_eval_deny_wins_over() {
    let rules = vec![
        Rule::new("bash", Effect::Deny).unwrap(),
        Rule::new("bash", Effect::Allow).unwrap(),
    ];
    assert_eq!(evaluate(&rules, "bash", "ls"), Some(Effect::Deny));
}

#[test]
fn test_eval_no_match_returns() {
    let rules = vec![Rule::new("bash", Effect::Allow).unwrap()];
    assert_eq!(evaluate(&rules, "edit", "/tmp/a"), None);
}

#[test]
fn test_eval_case_insensitive_tool() {
    let r = Rule::new("Bash", Effect::Allow).unwrap();
    assert_eq!(evaluate(from_ref(&r), "BASH", "ls"), Some(Effect::Allow));
}

#[test]
fn test_rule_rejects_empty_tool() {
    assert!(Rule::new("  ", Effect::Allow).is_err());
}

#[test]
fn test_content_exact_matches() {
    let r = Rule::with_content(
        "bash",
        RuleContent::Exact("npm install".into()),
        Effect::Allow,
    )
    .unwrap();
    assert_eq!(
        evaluate(from_ref(&r), "bash", "npm install"),
        Some(Effect::Allow)
    );
    // A different command does not match the exact content rule.
    assert_eq!(evaluate(from_ref(&r), "bash", "npm uninstall"), None);
}

#[test]
fn test_denied_agents_exact_deny() {
    let rules = vec![
        Rule::with_content("agent", RuleContent::Exact("explore".into()), Effect::Deny).unwrap(),
        Rule::with_content("agent", RuleContent::Exact("plan".into()), Effect::Deny).unwrap(),
        // An allow on a type does not deny it.
        Rule::with_content("agent", RuleContent::Exact("verify".into()), Effect::Allow).unwrap(),
        // A non-agent deny is irrelevant.
        Rule::new("bash", Effect::Deny).unwrap(),
        // A prefix agent deny is ignored — a type deny is always exact.
        Rule::with_content("agent", RuleContent::Prefix("ex".into()), Effect::Deny).unwrap(),
    ];
    let set = denied_agent_types(&rules);
    assert!(set.contains("explore"));
    assert!(set.contains("plan"));
    assert!(!set.contains("verify"));
    assert_eq!(set.len(), 2);
}

#[test]
fn test_content_prefix_matches() {
    let r = Rule::with_content("bash", RuleContent::Prefix("npm".into()), Effect::Allow).unwrap();
    assert_eq!(
        evaluate(from_ref(&r), "bash", "npm install"),
        Some(Effect::Allow)
    );
    assert_eq!(
        evaluate(from_ref(&r), "bash", "npm run build"),
        Some(Effect::Allow)
    );
    assert_eq!(evaluate(from_ref(&r), "bash", "cargo build"), None);
}

#[test]
fn test_content_glob_matches() {
    let r = Rule::with_content("bash", RuleContent::Glob("git *".into()), Effect::Allow).unwrap();
    assert_eq!(
        evaluate(from_ref(&r), "bash", "git commit -m x"),
        Some(Effect::Allow)
    );
    assert_eq!(
        evaluate(from_ref(&r), "bash", "git status"),
        Some(Effect::Allow)
    );
    assert_eq!(evaluate(from_ref(&r), "bash", "cargo build"), None);
}

#[test]
fn test_content_parse_colon_star() {
    match RuleContent::parse("npm:*") {
        RuleContent::Prefix(s) => assert_eq!(s, "npm"),
        other => panic!("expected prefix, got {other:?}"),
    }
}

#[test]
fn test_content_parse_detects_glob() {
    assert!(matches!(RuleContent::parse("git *"), RuleContent::Glob(_)));
    assert!(matches!(RuleContent::parse("ls?"), RuleContent::Glob(_)));
}

#[test]
fn test_plain_content_parses_exact() {
    assert!(matches!(
        RuleContent::parse("npm install"),
        RuleContent::Exact(_)
    ));
}

#[test]
fn test_content_glob_invalid_never() {
    // An unbalanced bracket is an invalid glob; fail-closed (no match).
    let r =
        Rule::with_content("bash", RuleContent::Glob("[invalid".into()), Effect::Allow).unwrap();
    assert_eq!(evaluate(from_ref(&r), "bash", "[invalid"), None);
}

#[test]
fn test_content_scoped_skips_empty() {
    // input None yields empty string, so a content-scoped rule never
    // matches a call with no input (tool-name-only rules still would).
    let r = Rule::with_content("bash", RuleContent::Prefix("npm".into()), Effect::Allow).unwrap();
    assert_eq!(evaluate(from_ref(&r), "bash", ""), None);
}

#[test]
fn test_tool_only_matches_any() {
    let r = Rule::new("bash", Effect::Allow).unwrap();
    assert_eq!(evaluate(from_ref(&r), "bash", ""), Some(Effect::Allow));
    assert_eq!(
        evaluate(from_ref(&r), "bash", "rm -rf /"),
        Some(Effect::Allow)
    );
}

#[test]
fn test_input_content_extracts_command() {
    let v = serde_json::json!({"command": "ls -la"});
    assert_eq!(input_content("bash", Some(&v)), "ls -la");
}

#[test]
fn test_input_content_extracts_path() {
    let v = serde_json::json!({"path": "/tmp/a.txt"});
    assert_eq!(input_content("edit", Some(&v)), "/tmp/a.txt");
}

#[test]
fn test_input_content_extracts_url() {
    let v = serde_json::json!({"url": "https://example.com"});
    assert_eq!(input_content("webfetch", Some(&v)), "https://example.com");
}

#[test]
fn test_input_content_extracts_skill() {
    let v = serde_json::json!({"skill": "deploy"});
    assert_eq!(input_content("skill", Some(&v)), "deploy");
    // A missing skill key yields empty, so a content rule never matches.
    let v2 = serde_json::json!({"args": "x"});
    assert_eq!(input_content("skill", Some(&v2)), "");
}

#[test]
fn test_input_content_none_empty() {
    assert_eq!(input_content("bash", None), "");
}

#[test]
fn test_effect_parse_roundtrip() {
    for e in [Effect::Allow, Effect::Deny, Effect::Ask] {
        assert_eq!(Effect::parse(e.label()).unwrap(), e);
    }
    assert!(Effect::parse("bogus").is_err());
}

#[test]
fn test_bash_prefix_single_command() {
    // A single command scopes to its first token.
    assert_eq!(bash_always_allow_prefix("ls -la /tmp"), Some("ls".into()));
}

#[test]
fn test_bash_prefix_runner_compound() {
    // Runners scope to runner+subcommand (tighter than runner alone).
    assert_eq!(
        bash_always_allow_prefix("npm run build"),
        Some("npm run".into())
    );
    assert_eq!(
        bash_always_allow_prefix("cargo test --lib"),
        Some("cargo test".into())
    );
}

#[test]
fn test_bash_prefix_destructive_allowed() {
    // Default-mode always-allow is the user's choice: rm/sudo/dd persist
    // a prefix rule (rm:*); the dangerous-rule stripper removes it in
    // classifier mode, and protected paths are guarded by the safety
    // layer regardless. The earlier "not eligible" refusal is dropped.
    assert_eq!(
        bash_always_allow_prefix("rm -rf /tmp/build"),
        Some("rm".into())
    );
    assert_eq!(
        bash_always_allow_prefix("sudo apt install foo"),
        Some("sudo".into())
    );
    assert_eq!(
        bash_always_allow_prefix("dd if=/dev/zero of=/dev/sda"),
        Some("dd".into())
    );
}

#[test]
fn test_bash_prefix_compound_refused() {
    // A compound (A && B) cannot be scoped by one prefix rule.
    assert_eq!(bash_always_allow_prefix("ls && rm -rf /tmp"), None);
    assert_eq!(bash_always_allow_prefix("echo hi || echo bye"), None);
}

#[test]
fn test_bash_prefix_runner_subcommand() {
    // A subcommand named like a destructive standalone command (cargo rm,
    // git rm) is the runner's version-controlled op, not the standalone
    // destructive command — scope to runner+subcommand, do not refuse.
    assert_eq!(
        bash_always_allow_prefix("cargo rm unused"),
        Some("cargo rm".into())
    );
    assert_eq!(
        bash_always_allow_prefix("git rm -f file"),
        Some("git rm".into())
    );
}
