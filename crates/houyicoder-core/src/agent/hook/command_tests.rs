//! Tests for CommandHook, split from command.rs on file-size grounds.

use super::*;
use houyicoder_api::launcher::{LauncherChild, LauncherExit, SpawnError, StdioPipes};
use houyicoder_context::SessionId;

/// A stub launcher whose command returns a canned verdict JSON on
/// stdout. The pipes are in-memory handles so the CommandHook
/// write-stdin / read-stdout path exercises end-to-end.
struct StubLauncher {
    stdout: String,
}
impl ProcessLauncher for StubLauncher {
    fn spawn(&self, _req: SpawnRequest, _policy: SpawnPolicy) -> Result<LauncherChild, SpawnError> {
        let stdout_buf = self.stdout.clone().into_bytes();
        let stdout: Box<dyn std::io::Read + Send> = Box::new(std::io::Cursor::new(stdout_buf));
        let stdin: Box<dyn std::io::Write + Send> = Box::new(std::io::sink());
        let pipes = StdioPipes {
            stdin: Some(stdin),
            stdout: Some(stdout),
            stderr: None,
        };
        Ok(LauncherChild::with_pipes(
            None,
            pipes,
            Box::pin(async move {
                Ok(LauncherExit {
                    exit_code: Some(0),
                    stdout: None,
                    stderr: None,
                })
            }),
        ))
    }
}

pub(crate) fn ctx_pre_tool_use() -> HookContext {
    HookContext {
        event: HookEvent::PreToolUse,
        payload: super::super::HookPayload::PreToolUse {
            tool_name: "recordable".into(),
            input: serde_json::json!({}),
            backfilled_input: None,
        },
        session: SessionId::new(),
    }
}

/// A matcher that does not match the tool name skips spawn (returns
/// Allow without calling the launcher).
#[test]
fn test_matcher_skip_returns_allow() {
    let launcher = Arc::new(StubLauncher {
        stdout: r#"{"verdict":"deny"}"#.into(),
    }) as Arc<dyn ProcessLauncher>;
    let hook = CommandHook::new(
        "cmd",
        vec![HookEvent::PreToolUse],
        "echo",
        vec![],
        launcher,
        HookSource::User,
    )
    .with_matcher("Bash");
    // ctx_pre_tool_use uses tool_name "recordable", not "Bash".
    let v = hook.evaluate(&ctx_pre_tool_use()).expect("evaluate");
    assert!(matches!(v, HookVerdict::Allow), "matcher skip -> Allow");
}

/// An if-condition that does not match skips spawn.
#[test]
fn test_if_skip_returns_allow() {
    let launcher = Arc::new(StubLauncher {
        stdout: r#"{"verdict":"deny"}"#.into(),
    }) as Arc<dyn ProcessLauncher>;
    let hook = CommandHook::new(
        "cmd",
        vec![HookEvent::PreToolUse],
        "echo",
        vec![],
        launcher,
        HookSource::User,
    )
    .with_if_condition("Bash(git *)");
    let v = hook.evaluate(&ctx_pre_tool_use()).expect("evaluate");
    assert!(matches!(v, HookVerdict::Allow), "if skip -> Allow");
}

/// A once hook self-unregisters after its first fire. The second
/// dispatch sees an empty registry.
#[test]
fn test_once_self_unregisters() {
    use super::super::registry::HookRegistry;
    let launcher = Arc::new(StubLauncher {
        stdout: r#"{"verdict":"allow"}"#.into(),
    }) as Arc<dyn ProcessLauncher>;
    let reg = Arc::new(HookRegistry::new());
    let hook = Arc::new(
        CommandHook::new(
            "once-cmd",
            vec![HookEvent::PreToolUse],
            "echo",
            vec![],
            launcher,
            HookSource::User,
        )
        .with_once()
        .with_registry(Arc::clone(&reg)),
    );
    let id = reg.register(hook.clone());
    hook.bind_hook_id(id);
    assert_eq!(reg.len(), 1, "hook registered");
    let outcomes = reg.dispatch(&ctx_pre_tool_use());
    assert!(!outcomes.is_empty(), "hook fired");
    drop(outcomes);
    assert_eq!(reg.len(), 0, "hook self-unregistered after fire");
}

/// A once hook whose spawn fails stays registered so a transient
/// failure can retry on the next event. Only a completed attempt
/// consumes the one shot.
#[test]
fn test_once_retries_after_failure() {
    use super::super::registry::HookRegistry;
    let reg = Arc::new(HookRegistry::new());
    let hook = Arc::new(
        CommandHook::new(
            "once-fail",
            vec![HookEvent::PreToolUse],
            "echo",
            vec![],
            Arc::new(FailingLauncher) as Arc<dyn ProcessLauncher>,
            HookSource::User,
        )
        .with_once()
        .with_registry(Arc::clone(&reg)),
    );
    let id = reg.register(hook.clone());
    hook.bind_hook_id(id);
    assert!(
        hook.evaluate(&ctx_pre_tool_use()).is_err(),
        "spawn failure surfaces as an error"
    );
    assert_eq!(reg.len(), 1, "a failed once hook stays registered");
    assert!(
        hook.evaluate(&ctx_pre_tool_use()).is_err(),
        "the retry actually spawns again instead of short-circuiting"
    );
    assert_eq!(reg.len(), 1, "still registered after a second failure");
}

#[tokio::test]
async fn test_deny_verdict_round_trips() {
    let launcher = Arc::new(StubLauncher {
        stdout: r#"{"verdict":"deny","reason":"blocked by command hook"}"#.into(),
    }) as Arc<dyn ProcessLauncher>;
    let hook = CommandHook::new(
        "cmd-deny",
        vec![HookEvent::PreToolUse],
        "echo",
        vec![],
        launcher,
        HookSource::Project,
    );
    let v = hook.evaluate(&ctx_pre_tool_use()).expect("evaluate");
    match v {
        HookVerdict::Deny(r) => assert_eq!(r, "blocked by command hook"),
        other => panic!("expected Deny, got {other:?}"),
    }
}

#[tokio::test]
async fn test_allow_verdict_round_trips() {
    let launcher = Arc::new(StubLauncher {
        stdout: r#"{"verdict":"allow"}"#.into(),
    }) as Arc<dyn ProcessLauncher>;
    let hook = CommandHook::new(
        "cmd-allow",
        vec![HookEvent::PreToolUse],
        "echo",
        vec![],
        launcher,
        HookSource::Project,
    );
    let v = hook.evaluate(&ctx_pre_tool_use()).expect("evaluate");
    assert!(matches!(v, HookVerdict::Allow));
}

#[tokio::test]
async fn test_unknown_verdict_not_allowed() {
    let launcher = Arc::new(StubLauncher {
        stdout: r#"{"verdict":"bogus"}"#.into(),
    }) as Arc<dyn ProcessLauncher>;
    let hook = CommandHook::new(
        "cmd-bogus",
        vec![HookEvent::PreToolUse],
        "echo",
        vec![],
        launcher,
        HookSource::Project,
    );
    let v = hook.evaluate(&ctx_pre_tool_use()).expect("evaluate");
    match v {
        HookVerdict::Observe(note) => assert!(
            note.contains("unknown verdict"),
            "observe names the misconfiguration: {note}"
        ),
        other => panic!("expected Observe, got {other:?}"),
    }
}

#[tokio::test]
async fn test_malformed_json_hook_error() {
    // stdout that starts with { but fails to parse is a malformed
    // verdict object, not the exit-code contract. The hook tried to
    // speak JSON and got it wrong, so an InvalidVerdict error surfaces
    // (the model can see which hook misconfigured itself). Plain
    // non-JSON stdout (no leading {) falls through to the exit-code
    // contract instead.
    let launcher = Arc::new(StubLauncher {
        stdout: "{not valid json".into(),
    }) as Arc<dyn ProcessLauncher>;
    let hook = CommandHook::new(
        "cmd-bad",
        vec![HookEvent::PreToolUse],
        "echo",
        vec![],
        launcher,
        HookSource::Project,
    );
    let err = hook
        .evaluate(&ctx_pre_tool_use())
        .expect_err("malformed json");
    assert!(matches!(err, HookError::InvalidVerdict { .. }));
    // Trait accessors (cover the name/events/source surface).
    assert_eq!(hook.name(), "cmd-bad");
    assert_eq!(hook.events(), &[HookEvent::PreToolUse]);
    assert_eq!(hook.source(), HookSource::Project);
}

fn ctx_post_tool_use() -> HookContext {
    HookContext {
        event: HookEvent::PostToolUse,
        payload: super::super::HookPayload::PostToolUse {
            tool_name: "recordable".into(),
            input: serde_json::json!({"x": 1}),
            result: super::super::ToolResult {
                output: "ok".into(),
            },
        },
        session: SessionId::new(),
    }
}

fn ctx_post_tool_use_failure() -> HookContext {
    HookContext {
        event: HookEvent::PostToolUseFailure,
        payload: super::super::HookPayload::PostToolUseFailure {
            tool_name: "recordable".into(),
            error: "boom".into(),
        },
        session: SessionId::new(),
    }
}

#[tokio::test]
async fn test_post_tool_use_payload() {
    // The PostToolUse branch of from_context carries the result field.
    let launcher = Arc::new(StubLauncher {
        stdout: r#"{"verdict":"allow"}"#.into(),
    }) as Arc<dyn ProcessLauncher>;
    let hook = CommandHook::new(
        "cmd-post",
        vec![HookEvent::PostToolUse],
        "echo",
        vec![],
        launcher,
        HookSource::Project,
    );
    let v = hook.evaluate(&ctx_post_tool_use()).expect("evaluate");
    assert!(matches!(v, HookVerdict::Allow));
}

#[tokio::test]
async fn test_post_tool_use_failure() {
    // The PostToolUseFailure branch of from_context carries the error.
    let launcher = Arc::new(StubLauncher {
        stdout: r#"{"verdict":"allow"}"#.into(),
    }) as Arc<dyn ProcessLauncher>;
    let hook = CommandHook::new(
        "cmd-fail",
        vec![HookEvent::PostToolUseFailure],
        "echo",
        vec![],
        launcher,
        HookSource::Project,
    );
    let v = hook
        .evaluate(&ctx_post_tool_use_failure())
        .expect("evaluate");
    assert!(matches!(v, HookVerdict::Allow));
}

#[tokio::test]
async fn test_trigger_verdict_round_trips() {
    // A Trigger verdict with a known event maps to HookVerdict::Trigger.
    let launcher = Arc::new(StubLauncher {
        stdout: r#"{"verdict":"trigger","event":"PreCompact"}"#.into(),
    }) as Arc<dyn ProcessLauncher>;
    let hook = CommandHook::new(
        "cmd-trigger",
        vec![HookEvent::PreToolUse],
        "echo",
        vec![],
        launcher,
        HookSource::Project,
    );
    let v = hook.evaluate(&ctx_pre_tool_use()).expect("evaluate");
    assert!(matches!(v, HookVerdict::Trigger(HookEvent::PreCompact)));
}

#[tokio::test]
async fn test_trigger_unknown_event_observed() {
    let launcher = Arc::new(StubLauncher {
        stdout: r#"{"verdict":"trigger","event":"NotAnEvent"}"#.into(),
    }) as Arc<dyn ProcessLauncher>;
    let hook = CommandHook::new(
        "cmd-trigger-bad",
        vec![HookEvent::PreToolUse],
        "echo",
        vec![],
        launcher,
        HookSource::Project,
    );
    let v = hook.evaluate(&ctx_pre_tool_use()).expect("evaluate");
    match v {
        HookVerdict::Observe(note) => assert!(
            note.contains("unknown event"),
            "observe names the misconfiguration: {note}"
        ),
        other => panic!("expected Observe, got {other:?}"),
    }
}

/// A launcher that refuses to spawn, so the spawn-error arm runs.
struct FailingLauncher;
impl ProcessLauncher for FailingLauncher {
    fn spawn(&self, _req: SpawnRequest, _policy: SpawnPolicy) -> Result<LauncherChild, SpawnError> {
        Err(SpawnError::Io("stub refuses spawn".into()))
    }
}

/// A launcher that records the policy it was handed and returns a canned
/// allow verdict, so a test can assert the spawn policy the hook built.
struct PolicyRecordingLauncher {
    policy: std::sync::Mutex<Option<SpawnPolicy>>,
    stdout: String,
}
impl ProcessLauncher for PolicyRecordingLauncher {
    fn spawn(&self, _req: SpawnRequest, policy: SpawnPolicy) -> Result<LauncherChild, SpawnError> {
        *self.policy.lock().unwrap() = Some(policy);
        let stdout: Box<dyn std::io::Read + Send> =
            Box::new(std::io::Cursor::new(self.stdout.clone().into_bytes()));
        let stdin: Box<dyn std::io::Write + Send> = Box::new(std::io::sink());
        let pipes = StdioPipes {
            stdin: Some(stdin),
            stdout: Some(stdout),
            stderr: None,
        };
        Ok(LauncherChild::with_pipes(
            None,
            pipes,
            Box::pin(async move {
                Ok(LauncherExit {
                    exit_code: Some(0),
                    stdout: None,
                    stderr: None,
                })
            }),
        ))
    }
}

#[tokio::test]
async fn test_hook_spawn_policy_audited() {
    // Every hook-command spawn must carry audit=true so an external command
    // the engine executes leaves a structured trace through the launcher
    // chokepoint, regardless of the hook's source.
    let launcher = Arc::new(PolicyRecordingLauncher {
        policy: std::sync::Mutex::new(None),
        stdout: r#"{"verdict":"allow"}"#.into(),
    });
    let policy_slot = Arc::clone(&launcher);
    let hook = CommandHook::new(
        "cmd-audit",
        vec![HookEvent::PreToolUse],
        "echo",
        vec![],
        launcher as Arc<dyn ProcessLauncher>,
        HookSource::User,
    );
    drop(hook.evaluate(&ctx_pre_tool_use()).expect("evaluate"));
    let captured = policy_slot
        .policy
        .lock()
        .unwrap()
        .clone()
        .expect("spawn ran");
    assert!(captured.audit, "hook spawn must be audited");
}

#[tokio::test]
async fn test_spawn_failure_hook_error() {
    let launcher = Arc::new(FailingLauncher) as Arc<dyn ProcessLauncher>;
    let hook = CommandHook::new(
        "cmd-nospawn",
        vec![HookEvent::PreToolUse],
        "echo",
        vec![],
        launcher,
        HookSource::Project,
    );
    let err = hook.evaluate(&ctx_pre_tool_use()).expect_err("spawn fails");
    assert!(matches!(err, HookError::ProcessError { .. }));
}
