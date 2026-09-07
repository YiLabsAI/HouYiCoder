//! Tests for the hook registry wiring, split from hook_compose.rs on
//! size grounds. Covers build_hook_registry dedup/last-wins/matcher/
//! event semantics and the SkillGrantHook trust contract.

use super::hook_compose::*;
use houyicoder_api::launcher::{ProcessLauncher, StdProcessLauncher};
use houyicoder_api::skill::{
    ProjectIdentity, SkillDescriptor, SkillError, SkillFamily, SkillProvenance, SkillRegistry,
    SkillScriptRef, SkillSnapshot, SkillSource,
};
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_config::HookSpec;
use houyicoder_core::agent::{
    HookContext, HookEvent, HookPayload, HookPolicy, HookSource, SkillTool, ToolResult,
};
use houyicoder_permission::{Effect, Scope};
use std::sync::Arc;

fn launcher() -> Arc<dyn ProcessLauncher> {
    Arc::new(StdProcessLauncher::new())
}

fn spec(name: &str, events: &[&str], program: &str) -> HookSpec {
    HookSpec {
        name: name.into(),
        events: events.iter().map(|s| (*s).into()).collect(),
        program: program.into(),
        args: Vec::new(),
        matcher: None,
        if_condition: None,
        shell: None,
        timeout_secs: None,
        once: false,
    }
}

fn user_spec(name: &str, events: &[&str], program: &str) -> (HookSpec, HookSource) {
    (spec(name, events, program), HookSource::User)
}

#[test]
fn test_build_hooks_empty() {
    assert!(build_hook_registry(&[], launcher(), HookPolicy::AllEnabled).is_none());
}

#[test]
fn test_build_hooks_valid() {
    let specs = vec![user_spec("lint", &["PreToolUse"], "true")];
    let reg =
        build_hook_registry(&specs, launcher(), HookPolicy::AllEnabled).expect("one valid hook");
    assert_eq!(reg.len(), 1);
}

#[test]
fn test_build_hooks_skips_bad() {
    // A spec with one known and one unknown event is skipped entirely;
    // a second valid spec still registers.
    let specs = vec![
        user_spec("bad", &["PreToolUse", "Nope"], "true"),
        user_spec("ok", &["PostToolUse"], "true"),
    ];
    let reg = build_hook_registry(&specs, launcher(), HookPolicy::AllEnabled)
        .expect("second spec registers");
    assert_eq!(reg.len(), 1);
}

#[test]
fn test_build_hooks_all_bad() {
    let specs = vec![user_spec("bad", &["DefinitelyNotAnEvent"], "true")];
    assert!(build_hook_registry(&specs, launcher(), HookPolicy::AllEnabled).is_none());
}

#[test]
fn test_build_hooks_no_events() {
    // A spec with an empty events list registers nothing (a hook that
    // fires on no event is a no-op; the typo is surfaced at registration,
    // not at fire time).
    let specs = vec![user_spec("idle", &[], "true")];
    assert!(build_hook_registry(&specs, launcher(), HookPolicy::AllEnabled).is_none());
}

#[test]
fn test_build_hooks_dedup() {
    // Two specs with the same program, args, shell, if condition,
    // events, and matcher are deduplicated: only one registers.
    let mut s1 = user_spec("lint-a", &["PreToolUse"], "true");
    s1.0.if_condition = Some("Bash".into());
    let mut s2 = user_spec("lint-b", &["PreToolUse"], "true");
    s2.0.if_condition = Some("Bash".into());
    let specs = vec![s1, s2];
    let reg =
        build_hook_registry(&specs, launcher(), HookPolicy::AllEnabled).expect("one after dedup");
    assert_eq!(reg.len(), 1);
}

#[test]
fn test_build_dedup_default_shell() {
    // An omitted shell and an explicit sh produce the identical
    // invocation, so a config that spells out the default must not
    // register a second copy that runs the same command twice.
    let mut s1 = user_spec("lint-implicit", &["PreToolUse"], "sh");
    s1.0.args = vec!["-c".into(), "echo hi".into()];
    s1.0.shell = None;
    let mut s2 = user_spec("lint-explicit", &["PreToolUse"], "sh");
    s2.0.args = vec!["-c".into(), "echo hi".into()];
    s2.0.shell = Some("sh".into());
    let specs = vec![s1, s2];
    let reg =
        build_hook_registry(&specs, launcher(), HookPolicy::AllEnabled).expect("one after dedup");
    assert_eq!(reg.len(), 1, "implicit and explicit sh are one hook");
}

#[test]
fn test_build_distinct_shells() {
    // A different shell is a different hook: the invocation differs,
    // so both must survive dedup.
    let mut s1 = user_spec("lint-sh", &["PreToolUse"], "sh");
    s1.0.args = vec!["-c".into(), "echo hi".into()];
    s1.0.shell = Some("sh".into());
    let mut s2 = user_spec("lint-bash", &["PreToolUse"], "bash");
    s2.0.args = vec!["-c".into(), "echo hi".into()];
    s2.0.shell = Some("bash".into());
    let specs = vec![s1, s2];
    let reg = build_hook_registry(&specs, launcher(), HookPolicy::AllEnabled).expect("two hooks");
    assert_eq!(reg.len(), 2, "sh and bash are distinct hooks");
}

#[test]
fn test_build_hooks_distinct_conditions() {
    // Same program but different if conditions are distinct hooks.
    let mut s1 = user_spec("lint-a", &["PreToolUse"], "true");
    s1.0.if_condition = Some("Bash".into());
    let mut s2 = user_spec("lint-b", &["PreToolUse"], "true");
    s2.0.if_condition = Some("Edit".into());
    let specs = vec![s1, s2];
    let reg = build_hook_registry(&specs, launcher(), HookPolicy::AllEnabled)
        .expect("two distinct hooks");
    assert_eq!(reg.len(), 2);
}

#[test]
fn test_build_hooks_cross_event() {
    // Same command, different events: both register. The dedup key
    // includes events so PreToolUse and PostToolUse are distinct.
    let s1 = user_spec("pre", &["PreToolUse"], "echo hi");
    let s2 = user_spec("post", &["PostToolUse"], "echo hi");
    let specs = vec![s1, s2];
    let reg = build_hook_registry(&specs, launcher(), HookPolicy::AllEnabled).expect("two hooks");
    assert_eq!(reg.len(), 2);
}

#[test]
fn test_build_hooks_distinct_matchers() {
    // Same command, same event, different matchers: both register.
    // The dedup key includes matcher so Bash and Edit are distinct.
    let mut s1 = user_spec("lint-bash", &["PreToolUse"], "echo hi");
    s1.0.matcher = Some("Bash".into());
    let mut s2 = user_spec("lint-edit", &["PreToolUse"], "echo hi");
    s2.0.matcher = Some("Edit".into());
    let specs = vec![s1, s2];
    let reg = build_hook_registry(&specs, launcher(), HookPolicy::AllEnabled).expect("two hooks");
    assert_eq!(reg.len(), 2);
}

#[test]
fn test_build_hooks_last_wins() {
    // Same hook from two sources: the last-seen source wins (project
    // overrides user). The winner is the project-sourced hook.
    let s1 = user_spec("lint-user", &["PreToolUse"], "echo hi");
    let s2 = (
        spec("lint-project", &["PreToolUse"], "echo hi"),
        HookSource::Project,
    );
    let specs = vec![s1, s2];
    let reg =
        build_hook_registry(&specs, launcher(), HookPolicy::AllEnabled).expect("one after dedup");
    assert_eq!(reg.len(), 1);
    // The winner should be the project hook (last-wins).
    let entries = reg.list();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].source, HookSource::Project);
}

#[test]
fn test_build_hooks_once_registers() {
    // A spec with once=true registers; the hook's once flag is wired.
    let mut s = user_spec("once-hook", &["PreToolUse"], "echo hi");
    s.0.once = true;
    let specs = vec![s];
    let reg =
        build_hook_registry(&specs, launcher(), HookPolicy::AllEnabled).expect("one once hook");
    assert_eq!(reg.len(), 1);
}

#[test]
fn test_build_if_cond_fires() {
    // A hook with if_condition="Bash" registers and fires when the
    // tool name matches. The if-condition is a bare tool name (no
    // glob pattern), so it passes on any Bash call.
    let mut s = user_spec("if-hook", &["PreToolUse"], "echo hi");
    s.0.if_condition = Some("Bash".into());
    let specs = vec![s];
    let reg = build_hook_registry(&specs, launcher(), HookPolicy::AllEnabled).expect("one if hook");
    assert_eq!(reg.len(), 1);
    // Dispatch the hook on a Bash PreToolUse event and verify it fires
    // (returns a verdict, not an Allow skip).
    use houyicoder_context::SessionId;
    let ctx = HookContext {
        event: HookEvent::PreToolUse,
        payload: HookPayload::PreToolUse {
            tool_name: "bash".into(),
            input: serde_json::json!({"command": "echo hi"}),
            backfilled_input: None,
        },
        session: SessionId::new(),
    };
    let outcomes = reg.dispatch(&ctx);
    assert_eq!(outcomes.len(), 1, "hook should fire on matching if");
}

/// After the Skill tool runs, the grant hook reads allowed_tools from a
/// trusted source's result (managed/user) and adds session-scoped Allow
/// rules so the granted tools do not re-ask during the skill execution.
#[test]
fn test_grant_adds_rules() {
    use houyicoder_context::SessionId;
    use houyicoder_permission::DefaultModeGate;

    let gate = Arc::new(DefaultModeGate::new());
    let hook = SkillGrantHook::new(gate.clone());

    let ctx = HookContext {
        event: HookEvent::PostToolUse,
        payload: HookPayload::PostToolUse {
            tool_name: "skill".to_string(),
            input: serde_json::json!({"skill":"commit"}),
            result: ToolResult {
                output: serde_json::json!({
                    "skill": "commit",
                    "result": "body",
                    "allowed_tools": ["Bash", "Read"],
                    "trusted": true,
                })
                .to_string(),
            },
        },
        session: SessionId::new(),
    };

    let verdict = hook.evaluate(&ctx).unwrap();
    assert!(matches!(verdict, HookVerdict::Allow), "grant hook allows");

    let rules = gate.rules();
    assert!(
        rules
            .iter()
            .any(|r| r.action == "Bash" && r.effect == Effect::Allow && r.scope == Scope::Session),
        "Bash session Allow rule added for a trusted source"
    );
    assert!(
        rules
            .iter()
            .any(|r| r.action == "Read" && r.effect == Effect::Allow && r.scope == Scope::Session),
        "Read session Allow rule added for a trusted source"
    );
}

/// A skill from a non-trusted source (project, ecosystem, local) must not
/// install session-scoped Allow rules for its tools: its allowed_tools
/// re-ask on each call so the user re-confirms. The trust flag false (or
/// absent) fails closed — nothing is granted.
#[test]
fn test_grant_skips_untrusted() {
    use houyicoder_context::SessionId;
    use houyicoder_permission::DefaultModeGate;

    let gate = Arc::new(DefaultModeGate::new());
    let hook = SkillGrantHook::new(gate.clone());

    let ctx = HookContext {
        event: HookEvent::PostToolUse,
        payload: HookPayload::PostToolUse {
            tool_name: "skill".to_string(),
            input: serde_json::json!({"skill":"deploy"}),
            result: ToolResult {
                output: serde_json::json!({
                    "skill": "deploy",
                    "result": "body",
                    "allowed_tools": ["Bash"],
                    "trusted": false,
                })
                .to_string(),
            },
        },
        session: SessionId::new(),
    };

    hook.evaluate(&ctx).unwrap();
    let rules = gate.rules();
    assert!(
        !rules
            .iter()
            .any(|r| r.action == "Bash" && r.scope == Scope::Session),
        "untrusted source: no session Allow rule for its tools"
    );

    // Absent trust flag fails closed too (a result the engine did not
    // produce, or an older shape).
    let ctx = HookContext {
        event: HookEvent::PostToolUse,
        payload: HookPayload::PostToolUse {
            tool_name: "skill".to_string(),
            input: serde_json::json!({"skill":"deploy"}),
            result: ToolResult {
                output: serde_json::json!({
                    "skill": "deploy",
                    "result": "body",
                    "allowed_tools": ["Bash"],
                })
                .to_string(),
            },
        },
        session: SessionId::new(),
    };
    hook.evaluate(&ctx).unwrap();
    assert!(
        !gate
            .rules()
            .iter()
            .any(|r| r.action == "Bash" && r.scope == Scope::Session),
        "absent trust flag: no session Allow rule"
    );

    // A string "true" (type confusion) fails closed too: the engine
    // always emits a bool, so a non-bool truthy value is a shape the
    // engine did not produce and grants nothing.
    let ctx = HookContext {
        event: HookEvent::PostToolUse,
        payload: HookPayload::PostToolUse {
            tool_name: "skill".to_string(),
            input: serde_json::json!({"skill":"deploy"}),
            result: ToolResult {
                output: serde_json::json!({
                    "skill": "deploy",
                    "result": "body",
                    "allowed_tools": ["Bash"],
                    "trusted": "true",
                })
                .to_string(),
            },
        },
        session: SessionId::new(),
    };
    hook.evaluate(&ctx).unwrap();
    assert!(
        !gate
            .rules()
            .iter()
            .any(|r| r.action == "Bash" && r.scope == Scope::Session),
        "string truthy value: no session Allow rule (fail closed on type confusion)"
    );
}

/// A non-skill tool does not trigger the grant.
#[test]
fn test_grant_skips_other() {
    use houyicoder_context::SessionId;
    use houyicoder_permission::DefaultModeGate;

    let gate = Arc::new(DefaultModeGate::new());
    let hook = SkillGrantHook::new(gate.clone());

    let ctx = HookContext {
        event: HookEvent::PostToolUse,
        payload: HookPayload::PostToolUse {
            tool_name: "bash".to_string(),
            input: serde_json::json!({}),
            result: ToolResult {
                output: "{}".to_string(),
            },
        },
        session: SessionId::new(),
    };

    let verdict = hook.evaluate(&ctx).unwrap();
    assert!(matches!(verdict, HookVerdict::Allow));
    assert!(
        !gate
            .rules()
            .iter()
            .any(|r| r.scope == Scope::Session && r.effect == Effect::Allow),
        "no session Allow rules added for non-skill tool"
    );
}

/// A stub registry for the contract test: returns one skill with a
/// configurable origin + allowed_tools, so the SkillTool to grant-hook
/// path runs end-to-end without touching the filesystem.
struct StubRegistry {
    origin: &'static str,
    allowed: Vec<String>,
}

impl SkillRegistry for StubRegistry {
    fn list_model_invocable(&self) -> Vec<SkillDescriptor> {
        Vec::new()
    }
    fn find(&self, _name: &str) -> Option<SkillDescriptor> {
        Some(SkillDescriptor {
            name: "s".into(),
            description: "stub".into(),
            when_to_use: None,
            argument_hint: None,
            disable_model_invocation: false,
            user_invocable: true,
            body_token_estimate: 4,
            allowed_tools: self.allowed.clone(),
            allowed_mach_services: Vec::new(),
            allow_app_launch: false,
        })
    }
    fn prepare_body(
        &self,
        _name: &str,
        _args: Option<&str>,
        _sid: Option<&str>,
    ) -> Result<String, SkillError> {
        Ok("body".into())
    }
    fn list_with_origin(&self) -> Vec<SkillSnapshot> {
        vec![SkillSnapshot {
            descriptor: self.find("s").unwrap(),
            origin: self.origin.into(),
            usage: Default::default(),
        }]
    }
    fn source_for(&self, _name: &str) -> Option<SkillSource> {
        let (family, provenance) = match self.origin {
            "managed" => (SkillFamily::Houyi, SkillProvenance::Managed),
            "ecosystem" => (SkillFamily::Agents, SkillProvenance::UserHome),
            _ => (
                SkillFamily::Houyi,
                SkillProvenance::Project(ProjectIdentity::from_canonical_root(
                    std::path::Path::new("/repo"),
                )),
            ),
        };
        Some(SkillSource::new(family, provenance))
    }
    fn detect_run_scripts(&self, _command: &str) -> Vec<SkillScriptRef> {
        Vec::new()
    }
}

/// The SkillTool to grant-hook JSON contract: a SkillTool result from a
/// trusted source drives the hook to add a session Allow rule, and an
/// untrusted source drives it to skip. Pins the trusted field name + shape
/// end-to-end so a rename at the producer fails the consumer here, not
/// silently in production.
#[tokio::test]
async fn test_grant_contract_runs_skilltool() {
    use houyicoder_context::SessionId;
    use houyicoder_permission::DefaultModeGate;

    let gate = Arc::new(DefaultModeGate::new());
    let hook = SkillGrantHook::new(gate.clone());
    let reg: Arc<dyn SkillRegistry> = Arc::new(StubRegistry {
        origin: "managed",
        allowed: vec!["Bash".into()],
    });
    let tool = SkillTool::new(reg);
    let out = tool
        .execute(ToolCtx::new("c1"), serde_json::json!({"skill":"s"}))
        .await
        .unwrap();
    let ctx = HookContext {
        event: HookEvent::PostToolUse,
        payload: HookPayload::PostToolUse {
            tool_name: "skill".to_string(),
            input: serde_json::json!({"skill":"s"}),
            result: ToolResult {
                output: out.to_string(),
            },
        },
        session: SessionId::new(),
    };
    hook.evaluate(&ctx).unwrap();
    assert!(
        gate.rules()
            .iter()
            .any(|r| r.action == "Bash" && r.scope == Scope::Session),
        "trusted source: SkillTool result drove the grant hook to add a rule"
    );

    let gate2 = Arc::new(DefaultModeGate::new());
    let hook2 = SkillGrantHook::new(gate2.clone());
    let reg2: Arc<dyn SkillRegistry> = Arc::new(StubRegistry {
        origin: "project",
        allowed: vec!["Bash".into()],
    });
    let tool2 = SkillTool::new(reg2);
    let out2 = tool2
        .execute(ToolCtx::new("c2"), serde_json::json!({"skill":"s"}))
        .await
        .unwrap();
    let ctx2 = HookContext {
        event: HookEvent::PostToolUse,
        payload: HookPayload::PostToolUse {
            tool_name: "skill".to_string(),
            input: serde_json::json!({"skill":"s"}),
            result: ToolResult {
                output: out2.to_string(),
            },
        },
        session: SessionId::new(),
    };
    hook2.evaluate(&ctx2).unwrap();
    assert!(
        !gate2
            .rules()
            .iter()
            .any(|r| r.action == "Bash" && r.scope == Scope::Session),
        "untrusted source: no session rule from the SkillTool result"
    );
}

#[tokio::test]
async fn test_ecosystem_tools_stay_gated() {
    use houyicoder_context::SessionId;
    use houyicoder_permission::DefaultModeGate;

    let registry: Arc<dyn SkillRegistry> = Arc::new(StubRegistry {
        origin: "ecosystem",
        allowed: vec!["Bash".into()],
    });
    let output = SkillTool::new(registry)
        .execute(ToolCtx::new("c3"), serde_json::json!({"skill":"s"}))
        .await
        .unwrap();
    assert_eq!(output["trusted"], false);
    let gate = Arc::new(DefaultModeGate::new());
    let hook = SkillGrantHook::new(gate.clone());
    hook.evaluate(&HookContext {
        event: HookEvent::PostToolUse,
        payload: HookPayload::PostToolUse {
            tool_name: "skill".into(),
            input: serde_json::json!({"skill":"s"}),
            result: ToolResult {
                output: output.to_string(),
            },
        },
        session: SessionId::new(),
    })
    .unwrap();
    assert!(
        !gate
            .rules()
            .iter()
            .any(|rule| rule.action == "Bash" && rule.scope == Scope::Session)
    );
}
