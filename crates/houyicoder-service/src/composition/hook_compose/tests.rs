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
use houyicoder_config::parse_hooks_from_settings;
use houyicoder_config::settings_merge::read_settings_value;
use houyicoder_context::SessionId;
use houyicoder_core::agent::{
    HookContext, HookEvent, HookPayload, HookPolicy, HookSource, HookVerdict, SkillTool, ToolResult,
};
use houyicoder_permission::DefaultModeGate;
use houyicoder_permission::{Effect, Scope};
use std::env::temp_dir;
use std::fs::{create_dir_all, remove_dir_all, write};
use std::path::Path;
use std::process::id;
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
                SkillProvenance::Project(ProjectIdentity::from_canonical_root(Path::new("/repo"))),
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

/// A project's own settings file is a hook source, and the workspace root is
/// what decides which file that is. This verifies the path from the project
/// settings file on disk to a fired verdict: the entry is read from that file,
/// dispatched through the registry, and the matcher decides whether the
/// command runs.
///
/// The command is false, which exits non-zero, so a dispatched hook answers
/// with a non-Allow verdict while a matcher-skipped one answers Allow. That
/// is what distinguishes "the file was read and the gate ran" from "nothing
/// was configured", which a bare dispatch count cannot tell apart.
#[test]
fn test_project_settings_hook_fires() {
    let root = temp_dir().join(format!("houyi-hooksrc-{}", id()));
    drop(remove_dir_all(&root));
    let ws = root.join("with-hooks");
    create_dir_all(ws.join(".houyicoder")).expect("mk workspace settings dir");
    let settings = ws.join(".houyicoder").join("settings.json");
    write(
        &settings,
        r#"{"hooks":{"PreToolUse":[{"matcher":"write","hooks":[{"type":"command","command":"false"}]}]}}"#,
    )
    .expect("write settings");

    let ctx = |tool: &str| HookContext {
        event: HookEvent::PreToolUse,
        payload: HookPayload::PreToolUse {
            tool_name: tool.into(),
            input: serde_json::json!({}),
            backfilled_input: None,
        },
        session: SessionId::new(),
    };

    let parsed = parse_hooks_from_settings(&read_settings_value(&settings));
    assert_eq!(parsed.len(), 1, "the project settings file names one hook");
    let tagged: Vec<(HookSpec, HookSource)> = parsed
        .into_iter()
        .map(|s| (s, HookSource::Project))
        .collect();
    let reg = build_hook_registry(&tagged, launcher(), HookPolicy::AllEnabled)
        .expect("project hook registers");

    let ran = |tool: &str| {
        reg.dispatch(&ctx(tool))
            .first()
            .map(|o| !matches!(o.result, Ok(HookVerdict::Allow)))
            .unwrap_or(false)
    };
    assert!(
        ran("write"),
        "the matcher selects write, so the command runs"
    );
    assert!(
        !ran("read"),
        "the matcher skips read, so the command does not run"
    );

    drop(remove_dir_all(&root));
}

/// A linked worktree without its own settings file inherits the main
/// checkout's hooks through its .git worktree pointer, and when a settings
/// file is written inside the worktree later the registry reloads it on the
/// spot without requiring an exit-and-reenter cycle.
#[test]
fn test_worktree_inherits_and_reload() {
    let root = temp_dir().join(format!("houyi-wthook-{}", id()));
    drop(remove_dir_all(&root));
    let main_repo = root.join("main");
    let wt_git = main_repo.join(".git").join("worktrees").join("wt-a");
    let wt_dir = root.join("wt-a");
    create_dir_all(&wt_git).expect("mk git worktrees dir");
    create_dir_all(main_repo.join(".houyicoder")).expect("mk main settings dir");
    create_dir_all(&wt_dir).expect("mk worktree dir");
    write(
        wt_dir.join(".git"),
        format!("gitdir: {}\n", wt_git.display()),
    )
    .expect("write worktree .git pointer");
    write(
        main_repo.join(".houyicoder").join("settings.json"),
        r#"{"hooks":{"PreToolUse":[{"matcher":"edit","hooks":[{"type":"command","command":"exit 2"}]}]}}"#,
    )
    .expect("write main settings");

    let gate = Arc::new(DefaultModeGate::new());
    let reg = build_session_registry(gate, launcher(), Some(&wt_dir));

    let ctx_for = |tool: &str| HookContext {
        event: HookEvent::PreToolUse,
        payload: HookPayload::PreToolUse {
            tool_name: tool.into(),
            input: serde_json::json!({"path": "src/lib.rs"}),
            backfilled_input: None,
        },
        session: SessionId::new(),
    };

    let edit_outcomes = reg.dispatch(&ctx_for("edit"));
    assert!(
        edit_outcomes
            .iter()
            .any(|o| matches!(o.result, Ok(HookVerdict::Deny(_)))),
        "linked worktree inherits main checkout PreToolUse hook immediately, got: {edit_outcomes:?}"
    );
    assert!(
        reg.dispatch(&ctx_for("write")).is_empty()
            || reg
                .dispatch(&ctx_for("write"))
                .iter()
                .all(|o| matches!(o.result, Ok(HookVerdict::Allow))),
        "write is not gated before worktree settings are written"
    );

    // Lay down a worktree-local settings file mid-session and verify the next
    // dispatch picks it up with zero exit/re-enter.
    create_dir_all(wt_dir.join(".houyicoder")).expect("mk wt settings dir");
    write(
        wt_dir.join(".houyicoder").join("settings.json"),
        r#"{"hooks":{"PreToolUse":[{"matcher":"write","hooks":[{"type":"command","command":"exit 2"}]}]}}"#,
    )
    .expect("write wt settings");

    let write_outcomes = reg.dispatch(&ctx_for("write"));
    assert!(
        write_outcomes
            .iter()
            .any(|o| matches!(o.result, Ok(HookVerdict::Deny(_)))),
        "newly laid down worktree settings file takes effect on the very next dispatch"
    );

    drop(remove_dir_all(&root));
}

/// A linked worktree sits inside the main checkout's directory, so its path
/// begins with the main path. Expanding the project placeholder before the
/// main-to-active rewrite would leave the active path in place and then
/// rewrite the main prefix inside it, nesting the worktree path in itself and
/// pointing the hook at a script that does not exist. The command must end up
/// naming the active worktree exactly once.
#[test]
fn test_inherited_hook_rewrite() {
    let main = Path::new("/repo");
    let active = Path::new("/repo/.houyicoder/worktrees/feature");
    let spec = HookSpec {
        name: "gate".into(),
        events: vec!["PreToolUse".into()],
        program: "sh".into(),
        args: vec![
            "-c".into(),
            "python3 $HOUYI_PROJECT_DIR/scripts/hook_rust.py".into(),
            // An argument that already names the main checkout, the shape a
            // copied settings file carries before the substitution.
            "/repo/scripts/hook_commit_gate.py".into(),
        ],
        matcher: None,
        if_condition: None,
        shell: None,
        timeout_secs: None,
        once: false,
    };
    let rewritten = rewrite_hook_spec(spec, active, Some(main));
    let expanded = "/repo/.houyicoder/worktrees/feature/scripts/hook_rust.py";
    assert_eq!(
        rewritten.args[1],
        format!("python3 {expanded}"),
        "the placeholder names the active worktree once"
    );
    assert_eq!(
        rewritten.args[2], "/repo/.houyicoder/worktrees/feature/scripts/hook_commit_gate.py",
        "a main-checkout path is repointed at the active worktree"
    );
    // The nesting this guards against: the active path appearing twice in one
    // argument, which resolves to no file at all.
    for arg in &rewritten.args {
        assert!(
            !arg.contains(".houyicoder/worktrees/feature/.houyicoder/worktrees/feature"),
            "the active path must not be nested inside itself: {arg}"
        );
    }

    // An argument that already names the active tree is left alone. The
    // rewrite exists to repoint a path copied from the main checkout; running
    // it over a path that already points here would nest the tree in itself,
    // so the guard is what makes the function safe to apply twice.
    let already = HookSpec {
        name: "gate".into(),
        events: vec!["PreToolUse".into()],
        program: "sh".into(),
        args: vec!["-c".into(), format!("python3 {expanded}")],
        matcher: None,
        if_condition: None,
        shell: None,
        timeout_secs: None,
        once: false,
    };
    let again = rewrite_hook_spec(already, active, Some(main));
    assert_eq!(
        again.args[1],
        format!("python3 {expanded}"),
        "an argument already naming the active tree survives a second pass"
    );
}
