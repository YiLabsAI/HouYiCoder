use super::*;
use crate::decision::{Decision, Outcome};
use crate::rule::{Effect, RuleContent};
use serde_json::Value;

fn edit_req(path: &str) -> ToolRequest<'_> {
    let v: &'static Value = Box::leak(serde_json::json!({"path": path}).into());
    ToolRequest {
        tool_name: "edit",
        input: Some(v),
        is_destructive: true,
        is_read_only: false,
        native_requires_approval: true,
    }
}

#[test]
fn test_manual_preserves_native() {
    // Manual asks before a tool that declares it needs approval; a read-only
    // tool still auto-allows.
    let g = DefaultModeGate::with_mode(PermissionMode::Manual);
    assert!(matches!(
        g.decide(&req("bash", true, false, true)).outcome(),
        Outcome::Ask
    ));
    assert!(matches!(
        g.decide(&req("read", false, true, false)).outcome(),
        Outcome::Allow
    ));
}

#[test]
fn test_auto_allows_safe_tools() {
    // Auto allows a destructive-flagged tool whose input carries no dangerous
    // content (the destructive gate keys on the command text, which is absent
    // here), so exec and filesystem both reach the Auto Allow default.
    let g = DefaultModeGate::with_mode(PermissionMode::Auto);
    assert!(matches!(
        g.decide(&req("bash", true, false, true)).outcome(),
        Outcome::Allow
    ));
    assert!(matches!(
        g.decide(&req("edit", true, false, true)).outcome(),
        Outcome::Allow
    ));
}

#[test]
fn test_auto_allows_plain_bash() {
    // Auto auto-approves a non-dangerous bash: a plain ls passed the
    // destructive (rm/sudo/redirect) and compound (&&/;/|) gates, so reaching
    // mode_default under Auto it is Allow.
    let g = DefaultModeGate::with_mode(PermissionMode::Auto);
    assert!(matches!(
        g.decide(&bash_req("ls -la /tmp")).outcome(),
        Outcome::Allow
    ));
    assert_eq!(g.decide(&bash_req("echo hello")).outcome(), Outcome::Allow);
}

#[test]
fn test_auto_asks_destructive_rm() {
    // Auto still asks for rm — should_ask_destructive fires before
    // mode_default, so a destructive command asks regardless of mode.
    let g = DefaultModeGate::with_mode(PermissionMode::Auto);
    assert!(matches!(
        g.decide(&bash_req("rm -rf /tmp/foo")).outcome(),
        Outcome::Ask
    ));
    assert!(matches!(
        g.decide(&bash_req("sudo apt install x")).outcome(),
        Outcome::Ask
    ));
}

#[test]
fn test_auto_compound_readonly_allow() {
    // Auto allows a read-only compound: find|wc's segments are attestable
    // (no redirect/backtick/$(...)) and the whole content has no rm/sudo,
    // so it reaches mode_default(Auto) Exec -> Allow.
    let g = DefaultModeGate::with_mode(PermissionMode::Auto);
    assert!(matches!(
        g.decide(&bash_req("find . -type f | wc -l")).outcome(),
        Outcome::Allow
    ));
    assert!(matches!(
        g.decide(&bash_req("ls -la | head -5")).outcome(),
        Outcome::Allow
    ));
}

#[test]
fn test_auto_compound_destructive_ask() {
    // Auto still asks when a compound's whole content carries a destructive
    // command — should_ask_destructive catches rm in the whole content before
    // mode_default, so rm|grep asks even in Auto.
    let g = DefaultModeGate::with_mode(PermissionMode::Auto);
    assert!(matches!(
        g.decide(&bash_req("rm -rf /tmp/x | grep foo")).outcome(),
        Outcome::Ask
    ));
    assert!(matches!(
        g.decide(&bash_req("sudo apt update && apt install x"))
            .outcome(),
        Outcome::Ask
    ));
}

#[test]
fn test_rule_whitelist_auto_allows() {
    let g = DefaultModeGate::with_mode(PermissionMode::Auto);
    g.add_rule(Rule::new("bash", Effect::Allow).unwrap());
    assert!(matches!(
        g.decide(&req("bash", true, false, true)).outcome(),
        Outcome::Allow
    ));
    assert!(matches!(
        g.decide(&req("edit", true, false, true)).outcome(),
        Outcome::Allow
    ));
    assert!(matches!(
        g.decide(&req("netfetch", true, false, true)).outcome(),
        Outcome::Ask
    ));
}

#[test]
fn test_allow_rule_blocks_destructive() {
    // Regression: a broad bash Allow rule must NOT auto-allow rm. The
    // destructive gate is immune to rules: rm/sudo escalate to Ask even when
    // an Allow rule matches. Before the pipeline reorder, the rule fired
    // first and rm -rf sailed through.
    let g = DefaultModeGate::with_mode(PermissionMode::Auto);
    g.add_rule(Rule::new("bash", Effect::Allow).unwrap());
    assert!(matches!(
        g.decide(&bash_req("rm -rf /tmp/x")).outcome(),
        Outcome::Ask
    ));
    assert!(matches!(
        g.decide(&bash_req("sudo rm /etc/passwd")).outcome(),
        Outcome::Ask
    ));
    // Plain non-destructive bash still honors the Allow rule.
    assert_eq!(g.decide(&bash_req("ls -la")).outcome(), Outcome::Allow);
}

#[test]
fn test_deny_rule_beats_destructive() {
    // A Deny rule is stricter than the destructive Ask, so deny short-
    // circuits: rm under a bash Deny rule is Deny, not Ask.
    let g = DefaultModeGate::with_mode(PermissionMode::Auto);
    g.add_rule(Rule::new("bash", Effect::Deny).unwrap());
    assert!(matches!(
        g.decide(&bash_req("rm -rf /tmp/x")).outcome(),
        Outcome::Deny
    ));
}

#[test]
fn test_destructive_consent_under_allow() {
    // The destructive gate is consent-overridable for an exact pre-approved
    // call: the user recorded consent for this exact rm command, so the
    // destructive gate lets it through under an Allow rule. A different rm
    // command (not the consented exact one) still asks.
    use crate::consent::{InMemoryConsentStore, args_key};
    let cs = Arc::new(InMemoryConsentStore::non_expiring());
    let v: &'static Value = Box::leak(serde_json::json!({"command": "rm -rf /tmp/x"}).into());
    cs.record("bash", &args_key(Some(v)));
    let g = DefaultModeGate::with_mode(PermissionMode::Auto).with_consent(cs);
    g.add_rule(Rule::new("bash", Effect::Allow).unwrap());
    assert!(matches!(
        g.decide(&bash_req("rm -rf /tmp/x")).outcome(),
        Outcome::Allow
    ));
    assert!(matches!(
        g.decide(&bash_req("rm -rf /tmp/different")).outcome(),
        Outcome::Ask
    ));
}

#[test]
fn test_rule_deny_overrides_mode() {
    let g = DefaultModeGate::with_mode(PermissionMode::Auto);
    g.add_rule(Rule::new("bash", Effect::Deny).unwrap());
    assert!(matches!(
        g.decide(&req("bash", true, false, true)).outcome(),
        Outcome::Deny
    ));
}

#[test]
fn test_manual_mode_methods() {
    // Manual: label/parse, no per-switch confirm, sandbox always mandatory,
    // tab-cycles to Auto.
    assert_eq!(PermissionMode::Manual.label(), "manual");
    assert!(matches!(
        PermissionMode::parse("manual"),
        Ok(PermissionMode::Manual)
    ));
    assert!(!PermissionMode::Manual.requires_confirm());
    assert!(PermissionMode::Manual.sandbox_mandatory());
    assert_eq!(
        PermissionMode::Manual.tab_next(),
        Some(PermissionMode::Auto)
    );
    assert_eq!(
        PermissionMode::Auto.tab_next(),
        Some(PermissionMode::Manual)
    );
}

#[test]
fn test_tab_cycle_manual_auto() {
    let g = DefaultModeGate::with_mode(PermissionMode::Manual);
    assert_eq!(g.tab_cycle().unwrap(), PermissionMode::Auto);
    assert_eq!(g.tab_cycle().unwrap(), PermissionMode::Manual);
}

#[test]
fn test_destructive_rm_asks() {
    let g = DefaultModeGate::with_mode(PermissionMode::Auto);
    assert!(matches!(
        g.decide(&bash_req("rm -rf /tmp/foo")).outcome(),
        Outcome::Ask
    ));
}

#[test]
fn test_destructive_sudo_asks() {
    let g = DefaultModeGate::with_mode(PermissionMode::Auto);
    assert_eq!(g.decide(&bash_req("sudo ls")).outcome(), Outcome::Ask);
}

#[test]
fn test_destructive_redirect_asks() {
    let g = DefaultModeGate::with_mode(PermissionMode::Auto);
    assert!(matches!(
        g.decide(&bash_req("echo hi > /tmp/x")).outcome(),
        Outcome::Ask
    ));
}

#[test]
fn test_destructive_consent_overrides_rm() {
    use crate::consent::{InMemoryConsentStore, args_key};
    use std::sync::Arc;
    let cs = Arc::new(InMemoryConsentStore::non_expiring());
    let g = DefaultModeGate::with_mode(PermissionMode::Auto).with_consent(cs);
    let v = serde_json::json!({"command": "rm -rf /tmp/foo"});
    let r = ToolRequest {
        tool_name: "bash",
        input: Some(&v),
        is_destructive: true,
        is_read_only: false,
        native_requires_approval: true,
    };
    assert_eq!(g.decide(&r).outcome(), Outcome::Ask);
    g.consent().unwrap().record("bash", &args_key(Some(&v)));
    assert_eq!(g.decide(&r).outcome(), Outcome::Allow);
}

#[test]
fn test_destructive_safe_cmd_allows() {
    let g = DefaultModeGate::with_mode(PermissionMode::Auto);
    assert!(matches!(
        g.decide(&bash_req("ls -la /tmp")).outcome(),
        Outcome::Allow
    ));
    assert_eq!(g.decide(&bash_req("echo hello")).outcome(), Outcome::Allow);
}

#[test]
fn test_destructive_protected_immune() {
    // rm on a protected path always asks (safety_check fires first).
    let g = DefaultModeGate::with_mode(PermissionMode::Auto);
    assert_eq!(g.decide(&bash_req("rm -rf .git/")).outcome(), Outcome::Ask);
}

#[test]
fn test_set_mode_records_history() {
    let g = DefaultModeGate::with_mode(PermissionMode::Manual);
    g.set_mode(PermissionMode::Auto, "test");
    g.set_mode(PermissionMode::Manual, "reset");
    let h = g.history();
    assert_eq!(h.len(), 2);
    assert_eq!(h[0].to, PermissionMode::Auto);
    assert_eq!(h[1].to, PermissionMode::Manual);
}

#[test]
fn test_mode_parse_roundtrip() {
    for m in [PermissionMode::Manual, PermissionMode::Auto] {
        assert_eq!(PermissionMode::parse(m.label()).unwrap(), m);
    }
    assert!(PermissionMode::parse("nope").is_err());
}

#[test]
fn test_no_rule_exec_asks() {
    let g = DefaultModeGate::with_mode(PermissionMode::Manual);
    assert!(matches!(
        g.decide(&req("bash", true, false, true)).outcome(),
        Outcome::Ask
    ));
}

#[test]
fn test_side_effect_none_allows() {
    let g = DefaultModeGate::new();
    assert!(matches!(
        g.decide(&req("read", false, true, false)).outcome(),
        Outcome::Allow
    ));
    assert!(matches!(
        g.decide(&req("ls", false, true, false)).outcome(),
        Outcome::Allow
    ));
}

#[test]
fn test_side_effect_exec_asks() {
    let g = DefaultModeGate::with_mode(PermissionMode::Manual);
    assert!(matches!(
        g.decide(&req("bash", true, false, true)).outcome(),
        Outcome::Ask
    ));
}

#[test]
fn test_side_effect_network_asks() {
    let g = DefaultModeGate::new();
    assert!(matches!(
        g.decide(&req("webfetch", false, false, true)).outcome(),
        Outcome::Ask
    ));
}

#[test]
fn test_side_effect_fs_asks() {
    let g = DefaultModeGate::with_mode(PermissionMode::Manual);
    assert!(matches!(
        g.decide(&req("edit", true, false, true)).outcome(),
        Outcome::Ask
    ));
}

#[test]
fn test_auto_allows_fs_exec() {
    // Auto allows filesystem writes and execution (the destructive and
    // compound gates in the pipeline already caught dangerous commands).
    let g = DefaultModeGate::with_mode(PermissionMode::Auto);
    assert!(matches!(
        g.decide(&req("edit", true, false, true)).outcome(),
        Outcome::Allow
    ));
    assert!(matches!(
        g.decide(&req("write", true, false, true)).outcome(),
        Outcome::Allow
    ));
    assert!(matches!(
        g.decide(&req("bash", false, false, false)).outcome(),
        Outcome::Allow
    ));
}

#[test]
fn test_consent_upgrades_ask() {
    use crate::consent::InMemoryConsentStore;
    use std::sync::Arc;
    let cs = Arc::new(InMemoryConsentStore::non_expiring());
    let g = DefaultModeGate::with_mode(PermissionMode::Manual).with_consent(cs);
    assert!(matches!(
        g.decide(&req("bash", true, false, true)).outcome(),
        Outcome::Ask
    ));
    g.consent().unwrap().record("bash", "");
    assert!(matches!(
        g.decide(&req("bash", true, false, true)).outcome(),
        Outcome::Allow
    ));
}

#[test]
fn test_consent_upgrades_rule_ask() {
    use crate::consent::InMemoryConsentStore;
    use std::sync::Arc;
    let cs = Arc::new(InMemoryConsentStore::non_expiring());
    let g = DefaultModeGate::new().with_consent(cs);
    g.add_rule(Rule::new("bash", Effect::Ask).unwrap());
    assert!(matches!(
        g.decide(&req("bash", true, false, true)).outcome(),
        Outcome::Ask
    ));
    g.consent().unwrap().record("bash", "");
    assert!(matches!(
        g.decide(&req("bash", true, false, true)).outcome(),
        Outcome::Allow
    ));
}

#[test]
fn test_consent_no_override_deny() {
    use crate::consent::InMemoryConsentStore;
    use std::sync::Arc;
    let cs = Arc::new(InMemoryConsentStore::non_expiring());
    let g = DefaultModeGate::new().with_consent(cs);
    g.add_rule(Rule::new("bash", Effect::Deny).unwrap());
    g.consent().unwrap().record("bash", "");
    assert!(matches!(
        g.decide(&req("bash", true, false, true)).outcome(),
        Outcome::Deny
    ));
}

#[test]
fn test_consent_keys_by_args() {
    use crate::consent::{InMemoryConsentStore, args_key};
    use std::sync::Arc;
    let cs = Arc::new(InMemoryConsentStore::non_expiring());
    let g = DefaultModeGate::with_mode(PermissionMode::Manual).with_consent(cs);
    let v = serde_json::json!({"path": "/tmp/a"});
    let r = ToolRequest {
        tool_name: "edit",
        input: Some(&v),
        is_destructive: true,
        is_read_only: false,
        native_requires_approval: true,
    };
    assert_eq!(g.decide(&r).outcome(), Outcome::Ask);
    g.consent().unwrap().record("edit", &args_key(Some(&v)));
    assert_eq!(g.decide(&r).outcome(), Outcome::Allow);
    let v2 = serde_json::json!({"path": "/tmp/b"});
    let r2 = ToolRequest {
        tool_name: "edit",
        input: Some(&v2),
        is_destructive: true,
        is_read_only: false,
        native_requires_approval: true,
    };
    assert_eq!(g.decide(&r2).outcome(), Outcome::Ask);
}

#[test]
fn test_expired_consent_asks() {
    use crate::consent::InMemoryConsentStore;
    use std::sync::Arc;
    use std::time::Duration;
    let cs = Arc::new(InMemoryConsentStore::new(Duration::from_millis(20)));
    let g = DefaultModeGate::with_mode(PermissionMode::Manual).with_consent(cs);
    g.consent().unwrap().record("bash", "");
    assert!(matches!(
        g.decide(&req("bash", true, false, true)).outcome(),
        Outcome::Allow
    ));
    std::thread::sleep(Duration::from_millis(30));
    assert!(matches!(
        g.decide(&req("bash", true, false, true)).outcome(),
        Outcome::Ask
    ));
}

#[test]
fn test_no_consent_falls_through() {
    let g = DefaultModeGate::with_mode(PermissionMode::Manual);
    assert!(matches!(
        g.decide(&req("bash", true, false, true)).outcome(),
        Outcome::Ask
    ));
    assert!(g.consent().is_none());
}

// --- Content, safety, and compound wiring tests ---

#[test]
fn test_content_rule_allows_specific() {
    let g = DefaultModeGate::new();
    g.add_rule(
        Rule::with_content("bash", RuleContent::Prefix("npm".into()), Effect::Allow).unwrap(),
    );
    assert!(matches!(
        g.decide(&bash_req("npm install")).outcome(),
        Outcome::Allow
    ));
    // A non-matching destructive command still asks via the destructive gate.
    assert_eq!(g.decide(&bash_req("rm -rf /")).outcome(), Outcome::Ask);
}

#[test]
fn test_content_deny_wins_specific() {
    // A tool-level allow plus a content-scoped deny: the deny wins for the
    // matching command, allow for the rest.
    let g = DefaultModeGate::with_mode(PermissionMode::Auto);
    g.add_rule(Rule::new("bash", Effect::Allow).unwrap());
    g.add_rule(Rule::with_content("bash", RuleContent::Prefix("rm".into()), Effect::Deny).unwrap());
    assert_eq!(g.decide(&bash_req("rm -rf /")).outcome(), Outcome::Deny);
    assert_eq!(g.decide(&bash_req("ls")).outcome(), Outcome::Allow);
}

#[test]
fn test_protected_git_asks() {
    // A protected path escalates to Ask with no rule.
    let g = DefaultModeGate::with_mode(PermissionMode::Auto);
    assert_eq!(g.decide(&bash_req("rm -rf .git/")).outcome(), Outcome::Ask);
    assert!(matches!(
        g.decide(&edit_req("/home/u/.houyicoder/settings.json"))
            .outcome(),
        Outcome::Ask
    ));
}

#[test]
fn test_protected_bashrc_asks() {
    let g = DefaultModeGate::with_mode(PermissionMode::Auto);
    assert!(matches!(
        g.decide(&bash_req("echo x >> ~/.bashrc")).outcome(),
        Outcome::Ask
    ));
}

#[test]
fn test_safety_no_false_positive() {
    let g = DefaultModeGate::with_mode(PermissionMode::Auto);
    assert!(matches!(
        g.decide(&bash_req("ls -la /tmp")).outcome(),
        Outcome::Allow
    ));
}

#[test]
fn test_unsafe_compound_asks() {
    // A redirect in the second segment escalates to Ask.
    let g = DefaultModeGate::with_mode(PermissionMode::Auto);
    assert!(matches!(
        g.decide(&bash_req("ls && echo hi > /tmp/x")).outcome(),
        Outcome::Ask
    ));
}

#[test]
fn test_safe_compound_allows() {
    // Two attestable segments, no redirect or substitution: Auto allows.
    let g = DefaultModeGate::with_mode(PermissionMode::Auto);
    assert!(matches!(
        g.decide(&bash_req("ls && echo hi")).outcome(),
        Outcome::Allow
    ));
}

#[test]
fn test_compound_unsafe_escalates_allow() {
    // An allow-rule on bash does not auto-allow an un-attestable compound
    // command; the gate escalates to Ask.
    let g = DefaultModeGate::with_mode(PermissionMode::Manual);
    g.add_rule(Rule::new("bash", Effect::Allow).unwrap());
    assert!(matches!(
        g.decide(&bash_req("ls && echo hi > /tmp/x")).outcome(),
        Outcome::Ask
    ));
    // An attestable compound command is allowed by the rule.
    assert!(matches!(
        g.decide(&bash_req("ls && echo hi")).outcome(),
        Outcome::Allow
    ));
}

#[test]
fn test_compound_unsafe_consent_overrides() {
    // Stored consent for the exact call upgrades the compound-safety Ask.
    use crate::consent::{InMemoryConsentStore, args_key};
    use std::sync::Arc;
    let cs = Arc::new(InMemoryConsentStore::non_expiring());
    let g = DefaultModeGate::with_mode(PermissionMode::Auto).with_consent(cs);
    let v = serde_json::json!({"command": "ls && echo hi > /tmp/x"});
    let r = ToolRequest {
        tool_name: "bash",
        input: Some(&v),
        is_destructive: true,
        is_read_only: false,
        native_requires_approval: true,
    };
    assert_eq!(g.decide(&r).outcome(), Outcome::Ask);
    g.consent().unwrap().record("bash", &args_key(Some(&v)));
    assert_eq!(g.decide(&r).outcome(), Outcome::Allow);
}

#[test]
fn test_consent_cannot_override_safety() {
    // Consent cannot upgrade a protected-path safety Ask.
    use crate::consent::{InMemoryConsentStore, args_key};
    use std::sync::Arc;
    let cs = Arc::new(InMemoryConsentStore::non_expiring());
    let g = DefaultModeGate::with_mode(PermissionMode::Auto).with_consent(cs);
    let v = serde_json::json!({"command": "rm -rf .git/"});
    let r = ToolRequest {
        tool_name: "bash",
        input: Some(&v),
        is_destructive: true,
        is_read_only: false,
        native_requires_approval: true,
    };
    g.consent().unwrap().record("bash", &args_key(Some(&v)));
    assert_eq!(g.decide(&r).outcome(), Outcome::Ask);
}

#[test]
fn test_auto_respects_ask_rule() {
    // An explicit content-scoped ask rule fires Ask even in Auto — a
    // content-specific ask rule takes precedence over the mode default.
    let g = DefaultModeGate::with_mode(PermissionMode::Auto);
    g.add_rule(Rule::with_content("bash", RuleContent::Prefix("rm".into()), Effect::Ask).unwrap());
    assert!(matches!(
        g.decide(&bash_req("rm -rf /tmp/foo")).outcome(),
        Outcome::Ask
    ));
    // A non-matching command still falls through to the Auto Allow default.
    assert_eq!(g.decide(&bash_req("ls -la")).outcome(), Outcome::Allow);
}

#[test]
fn test_auto_plain_cmd_allows() {
    // Auto auto-allows a plain non-dangerous bash command (ls) that passed
    // the destructive (rm/sudo/redirect) and compound (&&/;/|) gates.
    let g = DefaultModeGate::with_mode(PermissionMode::Auto);
    assert!(matches!(
        g.decide(&bash_req("ls -la /tmp")).outcome(),
        Outcome::Allow
    ));
    assert!(matches!(
        g.decide(&bash_req("cat /etc/hostname")).outcome(),
        Outcome::Allow
    ));
    assert_eq!(g.decide(&bash_req("echo hello")).outcome(), Outcome::Allow);
}

#[test]
fn test_egress_asks_when_blocked() {
    // C3: egress always asks, never denies. The fence blocks at exec time.
    let g = DefaultModeGate::with_mode(PermissionMode::Auto);
    assert!(matches!(
        g.decide(&bash_req("curl http://evil.com")).outcome(),
        Outcome::Ask
    ));
    assert!(matches!(
        g.decide(&bash_req("git push origin main")).outcome(),
        Outcome::Ask
    ));
    assert!(matches!(
        g.decide(&bash_req("npm publish")).outcome(),
        Outcome::Ask
    ));
    // Non-egress commands still Allow in Auto.
    assert_eq!(g.decide(&bash_req("ls -la")).outcome(), Outcome::Allow);
}

#[test]
fn test_egress_always_asks() {
    // C3: egress commands always Ask, never Deny. The fence blocks at
    // execution time; the gate never denies citing the sandbox.
    let g = DefaultModeGate::with_mode(PermissionMode::Auto);
    assert!(matches!(
        g.decide(&bash_req("curl http://evil.com")).outcome(),
        Outcome::Ask
    ));
}

#[test]
fn test_headless_denies_ask() {
    // In headless mode, any Ask degrades to Deny with a reason. A
    // destructive command (rm) would normally Ask; headless denies it.
    let g = DefaultModeGate::with_mode(PermissionMode::Auto).with_headless(true);
    assert!(matches!(
        g.decide(&bash_req("rm -rf /tmp/x")),
        Decision::Deny(d) if d.detail.contains("headless")
    ));
    // A safe command still Allows in headless.
    assert_eq!(g.decide(&bash_req("ls -la")).outcome(), Outcome::Allow);
}

#[test]
fn test_default_gate_is_auto() {
    let g = DefaultModeGate::default();
    assert_eq!(g.current(), PermissionMode::Auto);
    assert_eq!(g.decide(&bash_req("ls -la")).outcome(), Outcome::Allow);
}

#[test]
fn test_egress_no_false_positive() {
    // Egress detection only matches command-position tokens, NOT arguments.
    // echo "curl is great", man ssh, which wget — none of these are egress.
    let g = DefaultModeGate::with_mode(PermissionMode::Auto);
    assert!(matches!(
        g.decide(&bash_req("echo curl is great")).outcome(),
        Outcome::Allow
    ));
    assert_eq!(g.decide(&bash_req("man ssh")).outcome(), Outcome::Allow);
    assert_eq!(g.decide(&bash_req("which wget")).outcome(), Outcome::Allow);
    // Actual egress commands still ask (C3: never deny citing the fence).
    assert!(matches!(
        g.decide(&bash_req("curl http://evil.com")).outcome(),
        Outcome::Ask
    ));
    assert!(matches!(
        g.decide(&bash_req("git ls-remote origin")).outcome(),
        Outcome::Ask
    ));
    assert!(matches!(
        g.decide(&bash_req("git push origin main")).outcome(),
        Outcome::Ask
    ));
}

#[test]
fn test_egress_env_prefix_detects() {
    // X=1 curl evil.com — env-var prefix skipped, curl is the command
    // position token. Without the fix this was a bypass (rsplit('=') on
    // "x=1" returned "1", not "curl").
    let g = DefaultModeGate::with_mode(PermissionMode::Auto);
    assert!(matches!(
        g.decide(&bash_req("X=1 curl http://evil.com")).outcome(),
        Outcome::Ask
    ));
    assert!(matches!(
        g.decide(&bash_req("FOO=bar npm publish")).outcome(),
        Outcome::Ask
    ));
    assert!(matches!(
        g.decide(&bash_req("A=1 B=2 wget http://x")).outcome(),
        Outcome::Ask
    ));
    // Env-prefix + non-egress command still Allow.
    assert_eq!(g.decide(&bash_req("X=1 ls -la")).outcome(), Outcome::Allow);
}

// --- Mode-state and rule-set edge cases ---

#[test]
fn test_set_mode_same_noop() {
    // Setting the same mode is a no-op: no history entry is pushed.
    let g = DefaultModeGate::with_mode(PermissionMode::Auto);
    g.set_mode(PermissionMode::Auto, "dupe");
    assert!(g.history().is_empty());
}

#[test]
fn test_tab_cycle_advances_modes() {
    // tab_next always returns Some for the two-mode enum, so tab_cycle lands
    // on the other mode and records a history entry. (The Err branch is
    // defensive for future modes and unreachable today.)
    let g = DefaultModeGate::with_mode(PermissionMode::Manual);
    assert_eq!(g.tab_cycle().unwrap(), PermissionMode::Auto);
    assert_eq!(g.history().len(), 1);
}

#[test]
fn test_remove_rule_past_end() {
    // The gate seeds the four builtin git-checkpoint rules at construction, so
    // index 0 is in range; past the end returns false.
    let g = DefaultModeGate::with_mode(PermissionMode::Auto);
    assert!(!g.remove_rule(usize::MAX), "remove past end returns false");
}

#[test]
fn test_destructive_extended_set_asks() {
    // rmdir / unlink / dd / truncate escalate to Ask in Auto (the gate and
    // the snapshot trigger stay symmetric; mv and chmod -R are deferred).
    let g = DefaultModeGate::with_mode(PermissionMode::Auto);
    for cmd in [
        "rmdir foo",
        "unlink foo.txt",
        "dd if=/dev/zero of=x",
        "truncate -s 0 x",
    ] {
        assert!(
            g.decide(&bash_req(cmd)).outcome() == Outcome::Ask,
            "`{cmd}` should Ask"
        );
    }
}
