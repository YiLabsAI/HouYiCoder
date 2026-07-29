//! Tests for the exit-code contract (shell-style hooks that use exit
//! codes instead of the JSON verdict shape). Extracted from
//! hook_command.rs so that file stays under the file-size gate.

use super::*;
use houyicoder_api::launcher::{LauncherChild, LauncherExit, SpawnError, StdioPipes};

/// A stub launcher that returns non-JSON stdout plus an exit code and
/// stderr, simulating a shell-style hook that uses the exit-code contract
/// (exit 2 = Deny, exit 0 = Allow) rather than the JSON verdict shape.
pub(super) struct ExitCodeLauncher {
    pub(super) exit_code: i32,
    pub(super) stdout: String,
    pub(super) stderr: String,
}

impl ProcessLauncher for ExitCodeLauncher {
    fn spawn(&self, _req: SpawnRequest, _policy: SpawnPolicy) -> Result<LauncherChild, SpawnError> {
        let stdout_buf = self.stdout.clone().into_bytes();
        let stderr_buf = self.stderr.clone().into_bytes();
        let stdout: Box<dyn std::io::Read + Send> = Box::new(std::io::Cursor::new(stdout_buf));
        let stderr: Box<dyn std::io::Read + Send> = Box::new(std::io::Cursor::new(stderr_buf));
        let stdin: Box<dyn std::io::Write + Send> = Box::new(std::io::sink());
        let pipes = StdioPipes {
            stdin: Some(stdin),
            stdout: Some(stdout),
            stderr: Some(stderr),
        };
        let code = self.exit_code;
        Ok(LauncherChild::with_pipes(
            None,
            pipes,
            Box::pin(async move {
                Ok(LauncherExit {
                    exit_code: Some(code),
                    stdout: None,
                    stderr: None,
                })
            }),
        ))
    }
}

/// Re-use the pre-tool-use context the parent test module builds, so the
/// exit-code tests run against the same payload shape the runner fires.
fn ctx_pre_tool_use() -> HookContext {
    super::tests::ctx_pre_tool_use()
}

/// The exit-code contract: a shell-style hook that writes no JSON and
/// exits 2 with a stderr reason is interpreted as a Deny, with the
/// stderr as the model-visible reason so the agent can self-correct.
/// This is what makes a plain shell script a valid PreToolUse gate.
#[tokio::test]
async fn test_exit_two_deny_reason() {
    let launcher = Arc::new(ExitCodeLauncher {
        exit_code: 2,
        stdout: String::new(),
        stderr: "integration tests belong in tests/, see server_contract.rs".into(),
    }) as Arc<dyn ProcessLauncher>;
    let hook = CommandHook::new(
        "test-placement-gate",
        vec![HookEvent::PreToolUse],
        "sh",
        vec![],
        launcher,
        HookSource::User,
    );
    let v = hook.evaluate(&ctx_pre_tool_use()).expect("evaluate");
    match v {
        HookVerdict::Deny(r) => assert!(
            r.contains("integration tests belong in tests/"),
            "deny carries the stderr reason: {r}"
        ),
        other => panic!("expected Deny, got {other:?}"),
    }
}

/// exit 0 with non-JSON stdout is an Allow. A shell hook that only uses
/// exit codes (no JSON verdict) succeeds silently.
#[tokio::test]
async fn test_exit_code_zero_allow() {
    let launcher = Arc::new(ExitCodeLauncher {
        exit_code: 0,
        stdout: "ok\n".into(),
        stderr: String::new(),
    }) as Arc<dyn ProcessLauncher>;
    let hook = CommandHook::new(
        "cmd-sh",
        vec![HookEvent::PreToolUse],
        "sh",
        vec![],
        launcher,
        HookSource::User,
    );
    let v = hook.evaluate(&ctx_pre_tool_use()).expect("evaluate");
    assert!(matches!(v, HookVerdict::Allow), "exit 0 -> Allow");
}

/// A non-2 non-0 exit code (a misconfigured hook) surfaces as a
/// non-blocking Observe rather than a Deny — a misconfigured hook must
/// not brick the run by blocking every tool call.
#[tokio::test]
async fn test_exit_code_other_observe() {
    let launcher = Arc::new(ExitCodeLauncher {
        exit_code: 1,
        stdout: String::new(),
        stderr: "hook script failed".into(),
    }) as Arc<dyn ProcessLauncher>;
    let hook = CommandHook::new(
        "cmd-broken",
        vec![HookEvent::PreToolUse],
        "sh",
        vec![],
        launcher,
        HookSource::User,
    );
    let v = hook.evaluate(&ctx_pre_tool_use()).expect("evaluate");
    match v {
        HookVerdict::Observe(note) => assert!(
            note.contains("hook script failed"),
            "observe carries the stderr: {note}"
        ),
        other => panic!("expected Observe, got {other:?}"),
    }
}

/// When stdout is valid JSON, the JSON verdict wins over the exit code
/// (the structured contract takes precedence). This lets a hook use the
/// richer JSON shape even when it also sets an exit code.
#[tokio::test]
async fn test_json_verdict_wins_over() {
    let launcher = Arc::new(ExitCodeLauncher {
        exit_code: 2,
        stdout: r#"{"verdict":"allow"}"#.into(),
        stderr: String::new(),
    }) as Arc<dyn ProcessLauncher>;
    let hook = CommandHook::new(
        "cmd-json",
        vec![HookEvent::PreToolUse],
        "sh",
        vec![],
        launcher,
        HookSource::User,
    );
    let v = hook.evaluate(&ctx_pre_tool_use()).expect("evaluate");
    assert!(
        matches!(v, HookVerdict::Allow),
        "JSON verdict wins over exit code 2"
    );
}
