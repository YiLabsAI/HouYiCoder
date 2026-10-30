//! Tests for a command stopped at its wall-clock budget.

use super::super::*;
use houyicoder_context::{ExecConfig, ExecResult, SandboxError};
use std::env::temp_dir;
use std::path::Path;

/// A sandbox whose exec reports the budget running out, with whatever the
/// command had printed before the stop.
struct StoppedExec {
    stdout: String,
    stderr: String,
}

impl SandboxSession for StoppedExec {
    fn exec_with_config(
        &self,
        _c: &str,
        _cfg: ExecConfig,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        Box::pin(async {
            Err(SandboxError::Timeout {
                message: "wall-clock 120s exceeded".to_string(),
                stdout: self.stdout.clone(),
                stderr: self.stderr.clone(),
            })
        })
    }
    fn workspace_root(&self) -> Arc<Path> {
        Arc::from(temp_dir())
    }
}

/// A budget stop answers with what the command printed and says the budget
/// ran out. It must not come back as a tool error: the model reads an error as
/// a failed call and sends the same one again, or falls back to starting the
/// work in the background and polling it with sleep.
#[tokio::test]
async fn test_timeout_keeps_partial_output() {
    let tool = BashTool::new(Arc::new(StoppedExec {
        stdout: "compiled 12 of 40\n".into(),
        stderr: "warning: slow link\n".into(),
    }));
    let out = tool
        .execute(
            ToolCtx::new("c1"),
            serde_json::json!({"command": "cargo build"}),
        )
        .await
        .expect("a budget stop is a result, not a tool error");
    assert_eq!(out["timed_out"], true, "the stop is labelled: {out}");
    assert_eq!(out["success"], false, "the command did not finish: {out}");
    assert_eq!(
        out["stdout"], "compiled 12 of 40\n",
        "output printed before the stop is kept: {out}"
    );
    let stderr = out["stderr"].as_str().expect("stderr text");
    assert!(
        stderr.starts_with("bash: stopped, wall-clock 120s exceeded"),
        "the stop explains itself first: {stderr}"
    );
    assert!(
        stderr.contains("warning: slow link"),
        "the command's own stderr is kept after the notice: {stderr}"
    );
    assert!(
        out.get("exit_code").is_none(),
        "nothing exited, so no code is reported: {out}"
    );
}

/// A command that printed nothing before the stop still says why it ended,
/// so a silent build does not come back as an empty, unexplained result.
#[tokio::test]
async fn test_timeout_speaks_when_silent() {
    let tool = BashTool::new(Arc::new(StoppedExec {
        stdout: String::new(),
        stderr: String::new(),
    }));
    let out = tool
        .execute(ToolCtx::new("c1"), serde_json::json!({"command": "true"}))
        .await
        .expect("a budget stop is a result");
    assert_eq!(out["timed_out"], true, "the stop is labelled: {out}");
    assert_eq!(out["stdout"], "", "nothing was captured: {out}");
    assert_eq!(
        out["stderr"], "bash: stopped, wall-clock 120s exceeded",
        "the notice stands alone: {out}"
    );
}

/// Output too large for the context with no captured stream is bounded to a
/// head+tail stub, and the stop notice still reaches stderr in place of the
/// command's own, or the call comes back as a truncated blob with no cause.
#[tokio::test]
async fn test_truncation_keeps_stop_notice() {
    let tool = BashTool::new(Arc::new(StoppedExec {
        stdout: "a".repeat(BASH_MAX_OUTPUT_CHARS + 1),
        stderr: "warning: slow link\n".into(),
    }));
    let out = tool
        .execute(
            ToolCtx::new("c1"),
            serde_json::json!({"command": "cargo build"}),
        )
        .await
        .expect("a budget stop is a result");
    let stdout = out["stdout"].as_str().expect("stdout text");
    assert!(
        stdout.contains("Output truncated") && stdout.contains("not kept"),
        "the stub names the truncation and says the rest was not kept: {stdout}"
    );
    let stderr = out["stderr"].as_str().expect("stderr text");
    assert!(
        stderr.contains("bash: stopped, wall-clock 120s exceeded"),
        "the notice survives the truncation: {stderr}"
    );
    assert!(
        stderr.contains("warning: slow link"),
        "the command's own stderr is kept alongside the notice: {stderr}"
    );
}

/// Every other sandbox failure stays a tool error, so a missing backend or a
/// refused path is not mistaken for a command that merely ran out of time.
#[tokio::test]
async fn test_other_failure_propagates() {
    struct BrokenExec;

    impl SandboxSession for BrokenExec {
        fn exec_with_config(
            &self,
            _c: &str,
            _cfg: ExecConfig,
        ) -> PFut<'_, Result<ExecResult, SandboxError>> {
            Box::pin(async { Err(SandboxError::Unsupported("no backend".to_string())) })
        }
        fn workspace_root(&self) -> Arc<Path> {
            Arc::from(temp_dir())
        }
    }

    let tool = BashTool::new(Arc::new(BrokenExec));
    let err = tool
        .execute(ToolCtx::new("c1"), serde_json::json!({"command": "pwd"}))
        .await
        .expect_err("an unusable backend is a tool error");
    assert!(
        err.to_string().contains("bash:"),
        "the failure names the tool: {err}"
    );
}
