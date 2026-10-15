//! Tests for this module.

use super::*;
use crate::agent::{RunOutcome, Runner, RunnerConfig, ToolRegistry};
use crate::provider::test_support::FakeProvider;
use houyicoder_api::launcher::{LauncherChild, SpawnError};
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::PFut;
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::llm::Usage;
use houyicoder_protocol::llm::{CompletionResponse, OutputItem};
use houyicoder_resilience::Retry;
use houyicoder_session::SessionStore;

fn out(success: bool, stdout: &str, stderr: &str) -> MakeCheckOutput {
    MakeCheckOutput {
        success,
        stdout: stdout.into(),
        stderr: stderr.into(),
    }
}

// ---- MakeCheckGate output parsing ----

#[test]
fn test_parse_clippy_marker() {
    let o = out(
        false,
        "\u{1b}[0;33m\u{25b6} Running clippy...\u{1b}[0m\n\
         error: unused variable\n\
         \n\
         \u{1b}[0;31m\u{2717} clippy FAILED \u{2014} stopping here.\u{1b}[0m\n",
        "",
    );
    let f = parse_failures(&o);
    assert!(f.checks.iter().any(|c| c.contains("clippy")));
    assert!(!f.suggestions.is_empty());
}

#[test]
fn test_parse_naming() {
    let o = out(
        false,
        "naming: 2 violation(s)\n\u{2717} naming FAILED \u{2014} stopping here.\n",
        "",
    );
    let f = parse_failures(&o);
    assert!(f.checks.iter().any(|c| c.contains("naming")));
}

#[test]
fn test_parse_test_marker() {
    let o = out(
        false,
        "test foo ... FAILED\n\u{2717} test FAILED \u{2014} stopping here.\n",
        "",
    );
    let f = parse_failures(&o);
    assert!(f.checks.iter().any(|c| c.contains("tests: 1 failed")));
}

#[test]
fn test_parse_cargo_fallback() {
    // No check_code.sh marker (running cargo directly). The clippy error
    // line should still produce an entry.
    let o = out(false, "error: mismatched types\n", "");
    let f = parse_failures(&o);
    assert!(!f.checks.is_empty(), "should detect clippy/compiler errors");
}

#[test]
fn test_parse_success() {
    let o = out(true, "all good\n", "");
    let f = parse_failures(&o);
    assert!(f.checks.is_empty());
}

// ---- MakeCheckGate blocking verify ----

#[test]
fn test_gate_ok() {
    let gate = MakeCheckGate::workspace_root().with_runner(|| out(true, "", ""));
    assert!(gate.verify_blocking().is_ok());
}

#[test]
fn test_gate_fails() {
    let gate = MakeCheckGate::workspace_root().with_runner(|| {
        out(
            false,
            "error: unused\n\u{2717} clippy FAILED \u{2014} stopping here.\n",
            "",
        )
    });
    let err = gate.verify_blocking().expect_err("should fail");
    assert!(err.checks.iter().any(|c| c.contains("clippy")));
    assert!(!err.suggestions.is_empty());
}

#[test]
fn test_gate_closed() {
    // Non-zero exit but no recognizable marker: still a failure with a
    // generic entry, never a silent pass.
    let gate = MakeCheckGate::workspace_root().with_runner(|| out(false, "boom\n", ""));
    let err = gate.verify_blocking().expect_err("should fail");
    assert!(!err.checks.is_empty(), "must report something");
}

#[tokio::test]
async fn test_gate_async() {
    // The async trait method must agree with the blocking path.
    let gate = MakeCheckGate::workspace_root().with_runner(|| {
        out(
            false,
            "naming: 1 violation\n\u{2717} naming FAILED \u{2014} stopping here.\n",
            "",
        )
    });
    let store = SessionStore::new(Box::new(InMemoryBackend::new()));
    let err = gate
        .verify(SessionId::new(), &store)
        .await
        .expect_err("should fail");
    assert!(err.checks.iter().any(|c| c.contains("naming")));
}

#[test]
fn test_strip_ansi() {
    let s = "\u{1b}[0;31m\u{2717} clippy FAILED\u{1b}[0m";
    assert_eq!(strip_ansi(s), "\u{2717} clippy FAILED");
}

// ---- Launcher-based gate ----

/// A stub launcher that returns canned output, proving the gate routes
/// through the ProcessLauncher port instead of a direct Command.
struct StubLauncher {
    exit_code: i32,
    stdout: String,
    stderr: String,
}

impl ProcessLauncher for StubLauncher {
    fn spawn(&self, _req: SpawnRequest, _policy: SpawnPolicy) -> Result<LauncherChild, SpawnError> {
        let exit_code = self.exit_code;
        let stdout = self.stdout.clone();
        let stderr = self.stderr.clone();
        Ok(LauncherChild::new(
            None,
            Box::pin(async move {
                Ok(LauncherExit {
                    exit_code: Some(exit_code),
                    stdout: Some(stdout),
                    stderr: Some(stderr),
                })
            }),
        ))
    }
}

#[test]
fn test_gate_via_launcher_ok() {
    let launcher = Arc::new(StubLauncher {
        exit_code: 0,
        stdout: String::new(),
        stderr: String::new(),
    });
    let gate = MakeCheckGate::workspace_root().with_launcher(launcher);
    assert!(gate.verify_blocking().is_ok());
}

#[test]
fn test_gate_via_launcher_fail() {
    let launcher = Arc::new(StubLauncher {
        exit_code: 1,
        stdout: String::from("error: unused\n\u{2717} clippy FAILED \u{2014} stopping here.\n"),
        stderr: String::new(),
    });
    let gate = MakeCheckGate::workspace_root().with_launcher(launcher);
    let err = gate.verify_blocking().expect_err("should fail");
    assert!(err.checks.iter().any(|c| c.contains("clippy")));
}

#[test]
fn test_gate_via_launcher_closed() {
    let launcher = Arc::new(StubLauncher {
        exit_code: 1,
        stdout: String::from("boom\n"),
        stderr: String::new(),
    });
    let gate = MakeCheckGate::workspace_root().with_launcher(launcher);
    let err = gate.verify_blocking().expect_err("should fail");
    assert!(!err.checks.is_empty(), "must report something");
}

// ---- Runner verify-gate wiring ----

/// A test-only VerifyGate with a fixed result, so the runner wiring can be
/// exercised without spawning make. fail holds the outcome to return.
struct StubVerifyGate {
    fail: Option<VerifyFailure>,
}
impl VerifyGate for StubVerifyGate {
    fn verify(
        &self,
        _session: SessionId,
        _store: &dyn SessionLog,
    ) -> PFut<'_, Result<(), VerifyFailure>> {
        let fail = self.fail.clone();
        Box::pin(async move {
            match fail {
                None => Ok(()),
                Some(f) => Err(f),
            }
        })
    }
}

fn runner_with(provider: Arc<dyn ModelProvider>) -> Runner {
    runner_with_tools(provider, ToolRegistry::new())
}

fn runner_with_tools(provider: Arc<dyn ModelProvider>, tools: ToolRegistry) -> Runner {
    Runner::new(
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))) as Arc<dyn SessionLog>,
        provider,
        tools,
        RunnerConfig {
            model: "test".into(),
            instructions: "you are a test agent".into(),
            max_turns: 5,
            max_output_tokens: 8_000,
            retry: Retry {
                max_attempts: 2,
                ..Retry::default()
            },
        },
    )
}

/// A tool that requires approval, exercising the Interruption path.
struct GuardedTool;
impl Tool for GuardedTool {
    fn name(&self) -> &str {
        "guarded"
    }
    fn description(&self) -> &str {
        "a tool that needs human approval"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn execute(
        &self,
        _ctx: ToolCtx,
        input: serde_json::Value,
    ) -> PFut<'_, Result<serde_json::Value, ToolError>> {
        Box::pin(async move { Ok(serde_json::json!({"ran": input})) })
    }
    fn requires_approval(&self) -> bool {
        true
    }
}

#[tokio::test]
async fn test_runner_gate_ok() {
    // Gate returns Ok so the run surfaces FinalOutput unchanged.
    let p = Arc::new(FakeProvider::text("done"));
    let gate = Arc::new(StubVerifyGate { fail: None });
    let runner = runner_with(p).with_verify_gate(gate);
    let session = SessionId::new();
    let result = runner.run(session, "hi".into()).await.unwrap();
    match result.outcome {
        RunOutcome::FinalOutput(t) => assert_eq!(t, "done"),
        other => panic!("expected final output, got {other:?}"),
    }
}

#[tokio::test]
async fn test_runner_gate_fail() {
    // Gate returns Err so FinalOutput is replaced with VerifyFailed
    // carrying the gate findings, so the caller can re-prompt the model.
    let p = Arc::new(FakeProvider::text("done"));
    let gate = Arc::new(StubVerifyGate {
        fail: Some(VerifyFailure {
            checks: vec!["clippy -D warnings: 2 errors".into()],
            suggestions: vec!["run cargo clippy --fix".into()],
        }),
    });
    let runner = runner_with(p).with_verify_gate(gate);
    let session = SessionId::new();
    let result = runner.run(session, "hi".into()).await.unwrap();
    match result.outcome {
        RunOutcome::VerifyFailed(f) => {
            assert!(f.checks.iter().any(|c| c.contains("clippy")));
            assert!(!f.suggestions.is_empty());
        }
        other => panic!("expected verify failed, got {other:?}"),
    }
}

#[tokio::test]
async fn test_runner_no_gate() {
    // No gate installed means FinalOutput passes through (backward compat).
    let p = Arc::new(FakeProvider::text("done"));
    let runner = runner_with(p);
    let session = SessionId::new();
    let result = runner.run(session, "hi".into()).await.unwrap();
    assert!(matches!(result.outcome, RunOutcome::FinalOutput(_)));
}

#[tokio::test]
async fn test_runner_gate_skips_interrupt() {
    // Verify only fires on FinalOutput, not Interruption — the caller must
    // resolve approvals first. A failing gate on an interrupted run would
    // wrongly shadow the approval flow.
    let resp = CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: "c1".into(),
            name: "guarded".into(),
            input: serde_json::json!({}),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let p = Arc::new(FakeProvider::new(vec![resp]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(GuardedTool));
    let gate = Arc::new(StubVerifyGate {
        fail: Some(VerifyFailure {
            checks: vec!["should not run".into()],
            suggestions: vec![],
        }),
    });
    let runner = runner_with_tools(p, tools).with_verify_gate(gate);
    let session = SessionId::new();
    let result = runner.run(session, "hi".into()).await.unwrap();
    match result.outcome {
        RunOutcome::Interruption(a) => assert_eq!(a.len(), 1),
        other => panic!("expected interruption, got {other:?}"),
    }
}
