//! End-to-end proof that a command hook spawns a real process, pipes the
//! hook context JSON to its stdin, and parses the verdict JSON from stdout.
//! The stub-launcher tests in the core crate cover the verdict round-trips
//! and the spawn-error arm in isolation; these tests exercise the actual
//! std launcher + a real shell so the composition path is shown to work
//! outside a test double. Unix-only (no /bin/sh elsewhere).

#![cfg(unix)]

use houyicoder_api::launcher::{ProcessLauncher, StdProcessLauncher};
use houyicoder_context::SessionId;
use houyicoder_core::agent::{
    CommandHook, Hook, HookContext, HookEvent, HookPayload, HookRegistry, HookSource, HookVerdict,
};
use std::sync::Arc;

fn launcher() -> Arc<dyn ProcessLauncher> {
    Arc::new(StdProcessLauncher::new())
}

fn ctx_pre_tool_use() -> HookContext {
    HookContext {
        event: HookEvent::PreToolUse,
        payload: HookPayload::PreToolUse {
            tool_name: "example".into(),
            input: serde_json::json!({}),
            backfilled_input: None,
        },
        session: SessionId::new(),
    }
}

#[test]
fn test_deny_verdict_spawns() {
    // The shell drains stdin (so the payload write does not hit a closed
    // pipe) then prints a deny verdict the executor parses.
    let script = r#"cat >/dev/null; printf '{"verdict":"deny","reason":"blocked"}'"#;
    let hook = CommandHook::new(
        "deny-sh",
        vec![HookEvent::PreToolUse],
        "/bin/sh",
        vec!["-c".into(), script.into()],
        launcher(),
        HookSource::Project,
    );
    let v = hook.evaluate(&ctx_pre_tool_use()).expect("evaluate");
    match v {
        HookVerdict::Deny(r) => assert_eq!(r, "blocked"),
        other => panic!("expected Deny, got {other:?}"),
    }
}

#[test]
fn test_allow_verdict_spawns() {
    let script = r#"cat >/dev/null; printf '{"verdict":"allow"}'"#;
    let hook = CommandHook::new(
        "allow-sh",
        vec![HookEvent::PreToolUse],
        "/bin/sh",
        vec!["-c".into(), script.into()],
        launcher(),
        HookSource::Project,
    );
    let v = hook.evaluate(&ctx_pre_tool_use()).expect("evaluate");
    assert!(matches!(v, HookVerdict::Allow), "expected Allow");
}

/// A matcher that does not match the tool name skips spawn entirely —
/// the real shell is never invoked, so a deny script returns Allow.
#[test]
fn test_matcher_skip_no_spawn() {
    let script = r#"cat >/dev/null; printf '{"verdict":"deny","reason":"unreachable"}'"#;
    let hook = CommandHook::new(
        "matcher-skip",
        vec![HookEvent::PreToolUse],
        "/bin/sh",
        vec!["-c".into(), script.into()],
        launcher(),
        HookSource::User,
    )
    .with_matcher("Bash");
    // ctx_pre_tool_use uses tool_name "example", not "Bash".
    let v = hook.evaluate(&ctx_pre_tool_use()).expect("evaluate");
    assert!(
        matches!(v, HookVerdict::Allow),
        "matcher skip -> Allow, got {v:?}"
    );
}

/// An if-condition with a content pattern matches the tool's primary
/// content field. Bash(git *) fires when input.command starts with git.
#[test]
fn test_if_content_match_fires() {
    let script = r#"cat >/dev/null; printf '{"verdict":"deny","reason":"git blocked"}'"#;
    let hook = CommandHook::new(
        "if-git",
        vec![HookEvent::PreToolUse],
        "/bin/sh",
        vec!["-c".into(), script.into()],
        launcher(),
        HookSource::User,
    )
    .with_if_condition("Bash(git *)");
    let ctx = HookContext {
        event: HookEvent::PreToolUse,
        payload: HookPayload::PreToolUse {
            tool_name: "Bash".into(),
            input: serde_json::json!({"command": "git push origin main"}),
            backfilled_input: None,
        },
        session: SessionId::new(),
    };
    let v = hook.evaluate(&ctx).expect("evaluate");
    match v {
        HookVerdict::Deny(r) => assert_eq!(r, "git blocked"),
        other => panic!("expected Deny, got {other:?}"),
    }
}

/// An if-condition with a content pattern that does not match the
/// primary field skips spawn — a deny script is never invoked.
#[test]
fn test_if_content_mismatch_skips() {
    let script = r#"cat >/dev/null; printf '{"verdict":"deny","reason":"unreachable"}'"#;
    let hook = CommandHook::new(
        "if-npm",
        vec![HookEvent::PreToolUse],
        "/bin/sh",
        vec!["-c".into(), script.into()],
        launcher(),
        HookSource::User,
    )
    .with_if_condition("Bash(npm *)");
    let ctx = HookContext {
        event: HookEvent::PreToolUse,
        payload: HookPayload::PreToolUse {
            tool_name: "Bash".into(),
            input: serde_json::json!({"command": "git push"}),
            backfilled_input: None,
        },
        session: SessionId::new(),
    };
    let v = hook.evaluate(&ctx).expect("evaluate");
    assert!(
        matches!(v, HookVerdict::Allow),
        "if mismatch -> Allow, got {v:?}"
    );
}

/// A once hook fires on the first real spawn and self-unregisters from
/// the registry. A second dispatch sees an empty registry.
#[test]
fn test_once_fires_real_spawn() {
    let script = r#"cat >/dev/null; printf '{"verdict":"allow"}'"#;
    let reg = Arc::new(HookRegistry::new());
    let hook = Arc::new(
        CommandHook::new(
            "once-real",
            vec![HookEvent::PreToolUse],
            "/bin/sh",
            vec!["-c".into(), script.into()],
            launcher(),
            HookSource::User,
        )
        .with_once()
        .with_registry(Arc::clone(&reg)),
    );
    let id = reg.register(hook.clone());
    hook.bind_hook_id(id);
    assert_eq!(reg.len(), 1, "hook registered");

    let outcomes = reg.dispatch(&ctx_pre_tool_use());
    assert_eq!(outcomes.len(), 1, "one hook fired");
    assert_eq!(reg.len(), 0, "hook self-unregistered after fire");

    // Second dispatch: registry is empty, no hook fires.
    let outcomes2 = reg.dispatch(&ctx_pre_tool_use());
    assert!(outcomes2.is_empty(), "second dispatch: no hooks");
}

/// A regex matcher matches the tool name via the precompiled regex.
#[test]
fn test_matcher_regex_fires() {
    let script = r#"cat >/dev/null; printf '{"verdict":"deny","reason":"regex matched"}'"#;
    let hook = CommandHook::new(
        "matcher-regex",
        vec![HookEvent::PreToolUse],
        "/bin/sh",
        vec!["-c".into(), script.into()],
        launcher(),
        HookSource::User,
    )
    .with_matcher("^(Bash|Edit)$");
    let ctx = HookContext {
        event: HookEvent::PreToolUse,
        payload: HookPayload::PreToolUse {
            tool_name: "Bash".into(),
            input: serde_json::json!({}),
            backfilled_input: None,
        },
        session: SessionId::new(),
    };
    let v = hook.evaluate(&ctx).expect("evaluate");
    match v {
        HookVerdict::Deny(r) => assert_eq!(r, "regex matched"),
        other => panic!("expected Deny, got {other:?}"),
    }
}
