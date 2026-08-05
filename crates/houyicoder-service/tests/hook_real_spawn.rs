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
    CommandHook, Hook, HookContext, HookEvent, HookPayload, HookSource, HookVerdict,
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
