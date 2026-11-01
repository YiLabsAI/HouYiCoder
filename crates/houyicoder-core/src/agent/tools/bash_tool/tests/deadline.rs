//! Which clock ends a foreground wait: the command's own timeout answers the
//! call, and the handoff budget only moves a command still running past it.

use super::super::*;
use super::background::RecordingLauncher;
use houyicoder_api::background::BackgroundCommandLauncher;
use houyicoder_context::{ExecConfig, ExecResult, SandboxError};
use serde_json::json;
use std::env::temp_dir;
use std::future::ready;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

/// A sandbox that reports a timeout the way a backend with an elapsed
/// wall-clock limit does, and records the limit it was given.
struct TimeoutExec {
    recorded_timeout_ms: Mutex<Option<u64>>,
}

impl SandboxSession for TimeoutExec {
    fn exec_with_config(
        &self,
        _command: &str,
        config: ExecConfig,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        *self.recorded_timeout_ms.lock().expect("exec lock") = Some(config.wall_timeout_ms);
        Box::pin(async {
            Err(SandboxError::Timeout {
                message: "wall-clock 2s exceeded".to_string(),
                stdout: "partial build output\n".to_string(),
                stderr: String::new(),
            })
        })
    }

    fn workspace_root(&self) -> Arc<Path> {
        Arc::from(temp_dir())
    }
}

fn finished() -> PFut<'static, Result<ExecResult, SandboxError>> {
    Box::pin(async {
        Ok(ExecResult {
            stdout: "done\n".into(),
            stderr: String::new(),
            exit_code: Some(0),
        })
    })
}

/// A command that has already answered is reported as finished even with a
/// zero budget: a finished run is never moved to the background.
#[tokio::test]
async fn test_finished_run_answers() {
    let mut exec = finished();
    let tail = CommandTail::new();
    let pick = wait_for_run(
        &mut exec,
        None,
        Some(Duration::ZERO),
        None,
        &tail,
        Instant::now(),
        2_000,
    )
    .await;
    match pick {
        RunPick::Done(Ok(result)) => assert_eq!(result.exit_code, Some(0), "the run's own answer"),
        RunPick::Done(Err(error)) => panic!("expected a finished command: {error:?}"),
        RunPick::Budget => panic!("a finished command answers the call instead of moving back"),
        RunPick::Cancelled => panic!("nothing cancelled the run"),
    }
}

/// A command that answers at the same instant its budget elapses still
/// answers the call. Repeated because an unordered select rotates the branch
/// it starts from, so a single round cannot tell a fixed order from luck.
#[tokio::test]
async fn test_answer_outranks_budget() {
    let tail = CommandTail::new();
    for round in 0..128 {
        let mut exec = finished();
        let pick = wait_for_outcome(
            &mut exec,
            None,
            ready(()),
            None,
            &tail,
            Instant::now(),
            2_000,
        )
        .await;
        assert!(
            matches!(pick, RunPick::Done(Ok(_))),
            "round {round}: the answer is offered before the elapsed budget"
        );
    }
}

/// A command stopped by its own shorter timeout answers the tool call while
/// the handoff budget is still far off: the timeout reaches the backend as a
/// wall-clock limit, and the host is given no run to keep.
#[tokio::test]
async fn test_own_timeout_stays_foreground() {
    let session = Arc::new(TimeoutExec {
        recorded_timeout_ms: Mutex::new(None),
    });
    let launcher = Arc::new(RecordingLauncher::default());
    let tool = BashTool::new(session.clone()).with_background_commands(
        Arc::clone(&launcher) as Arc<dyn BackgroundCommandLauncher>,
        Duration::from_secs(15),
    );
    let out = tool
        .execute(
            ToolCtx::new("c1"),
            json!({"command": "cargo build", "timeout_ms": 2_000}),
        )
        .await
        .expect("a command that reached its own timeout answers with a result");
    assert_eq!(
        *session.recorded_timeout_ms.lock().expect("exec lock"),
        Some(2_000),
        "the command's own timeout reaches the backend"
    );
    assert_eq!(
        out["timed_out"], true,
        "the stop is the command's own: {out}"
    );
    assert!(
        out.get("background_task").is_none(),
        "a stopped command is not moved to the background: {out}"
    );
    assert!(
        launcher.take().is_empty(),
        "the host is given no run to keep"
    );
}
