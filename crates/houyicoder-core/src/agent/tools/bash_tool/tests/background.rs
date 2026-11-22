//! Tests for a command that outlives its foreground budget.

use super::super::*;
use houyicoder_api::background::{
    BackgroundCommandLauncher, BackgroundRefusal, LaunchedTask, RunningTaskSummary,
};
use houyicoder_api::capture::CapturedStream;
use houyicoder_api::spawn::AgentIdentity;
use houyicoder_context::{ExecConfig, ExecResult, SandboxError};
use houyicoder_protocol::frontend::BackgroundTaskId;
use std::env::temp_dir;
use std::mem::take;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

/// A session that prints one line and then keeps working until the test says
/// it may end, so the foreground budget always expires first.
struct SlowExec {
    release: Arc<Notify>,
}

impl SandboxSession for SlowExec {
    fn exec_with_config(
        &self,
        _command: &str,
        _config: ExecConfig,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        Box::pin(async {
            self.release.notified().await;
            Ok(ExecResult {
                stdout: "done\n".into(),
                stderr: String::new(),
                exit_code: Some(0),
            })
        })
    }

    fn exec_streaming_captured(
        &self,
        command: &str,
        _lines: Arc<AtomicI64>,
        config: ExecConfig,
        receiver: Option<Arc<dyn RawOutputReceiver>>,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        if let Some(receiver) = receiver {
            receiver.receive(CapturedStream::Stdout, b"building 1 of 2\n");
        }
        self.exec_with_config(command, config)
    }

    fn workspace_root(&self) -> Arc<Path> {
        Arc::from(temp_dir())
    }
}

/// A launcher that keeps every background run so the test can finish it.
#[derive(Default)]
pub(super) struct RecordingLauncher {
    launched: Mutex<Vec<DetachedCommand>>,
    refusal: Mutex<Option<BackgroundRefusal>>,
}

impl RecordingLauncher {
    pub(super) fn take(&self) -> Vec<DetachedCommand> {
        take(&mut *self.launched.lock().expect("launcher lock"))
    }

    fn refuse_with(&self, refusal: BackgroundRefusal) {
        *self.refusal.lock().expect("refusal lock") = Some(refusal);
    }
}

impl BackgroundCommandLauncher for RecordingLauncher {
    fn check_admission(&self, _command: &str) -> Result<(), BackgroundRefusal> {
        if let Some(refusal) = self.refusal.lock().expect("refusal lock").clone() {
            return Err(refusal);
        }
        Ok(())
    }

    fn try_launch(
        &self,
        command: DetachedCommand,
        _reject_duplicate: bool,
    ) -> Result<LaunchedTask, (DetachedCommand, BackgroundRefusal)> {
        if let Some(refusal) = self.refusal.lock().expect("refusal lock").clone() {
            return Err((command, refusal));
        }
        Ok(LaunchedTask {
            id: self.launch(command),
            duplicate: None,
        })
    }

    fn launch(&self, command: DetachedCommand) -> BackgroundTaskId {
        let mut launched = self.launched.lock().expect("launcher lock");
        launched.push(command);
        BackgroundTaskId(launched.len() as u64)
    }
}

fn slow_session() -> (Arc<SlowExec>, Arc<Notify>) {
    let release = Arc::new(Notify::new());
    (
        Arc::new(SlowExec {
            release: Arc::clone(&release),
        }),
        release,
    )
}

/// A command still running when the budget ends is passed over exactly once,
/// and the call answers immediately with the task number and the live tail
/// instead of waiting out the command.
#[tokio::test]
async fn test_budget_hands_off() {
    let (session, release) = slow_session();
    let launcher = Arc::new(RecordingLauncher::default());
    let tool = BashTool::new(session).with_background_commands(
        Arc::clone(&launcher) as Arc<dyn BackgroundCommandLauncher>,
        Duration::from_millis(10),
    );

    let out = tool
        .execute(
            ToolCtx::new("c1"),
            serde_json::json!({"command": "cargo build"}),
        )
        .await
        .expect("a background call answers rather than failing");

    assert_eq!(out["background_task"], serde_json::json!(1));
    assert!(
        out["stderr"]
            .as_str()
            .is_some_and(|text| text.contains("task #1")),
        "the answer must name the task it was given: {out}"
    );
    assert_eq!(out["tail"], serde_json::json!(["building 1 of 2"]));

    let mut launched = launcher.take();
    assert_eq!(launched.len(), 1, "the run is passed over exactly once");
    let detached = launched.pop().expect("one background run");
    assert_eq!(detached.command, "cargo build");

    release.notify_one();
    let completion = detached.completion.await;
    assert_eq!(completion.exit_code, Some(0));
    assert!(completion.is_success());
}

/// A zero budget turns the background move off: every command finishes in the
/// foreground, which is what a non-interactive run wants.
#[tokio::test]
async fn test_zero_budget_stays_foreground() {
    let (session, release) = slow_session();
    let launcher = Arc::new(RecordingLauncher::default());
    let tool = BashTool::new(session).with_background_commands(
        Arc::clone(&launcher) as Arc<dyn BackgroundCommandLauncher>,
        Duration::ZERO,
    );

    let run = tool.execute(
        ToolCtx::new("c1"),
        serde_json::json!({"command": "cargo build"}),
    );
    release.notify_one();
    let out = run.await.expect("the command answers the call");
    assert!(out.get("background_task").is_none());
    assert_eq!(out["exit_code"], serde_json::json!(0));
    assert!(launcher.take().is_empty());
}

/// Cancelling the run after the background move does not kill the command the user
/// was told is still running: the detached run never inherits the parent's
/// cancel token.
#[tokio::test]
async fn test_background_move_ignores_cancel() {
    let (session, release) = slow_session();
    let launcher = Arc::new(RecordingLauncher::default());
    let tool = BashTool::new(session).with_background_commands(
        Arc::clone(&launcher) as Arc<dyn BackgroundCommandLauncher>,
        Duration::from_millis(10),
    );
    let cancel = CancellationToken::new();

    tool.execute(
        ToolCtx::new("c1").with_cancel(cancel.clone()),
        serde_json::json!({"command": "cargo build"}),
    )
    .await
    .expect("the call answers at the budget");

    let mut launched = launcher.take();
    let detached = launched.pop().expect("one background run");
    cancel.cancel();
    release.notify_one();

    let completion = detached.completion.await;
    assert!(
        completion.is_success(),
        "an Esc on the next turn must not end a background command"
    );
}

/// A child agent's command stays with the child.
///
/// The child is what the parent can hand over, stop, or end on a ceiling, and
/// each of those must reach the command it is running. A command detached into
/// the session's task registry would be beyond all three, so a child's call
/// keeps its command in the foreground even when the host offers a budget.
#[tokio::test]
async fn test_child_command_stays_foreground() {
    let (session, release) = slow_session();
    let launcher = Arc::new(RecordingLauncher::default());
    let tool = Arc::new(BashTool::new(session).with_background_commands(
        Arc::clone(&launcher) as Arc<dyn BackgroundCommandLauncher>,
        Duration::from_millis(10),
    ));

    // The command is still running when the budget expires, which is the only
    // state in which the hand-over could happen at all.
    let running = {
        let tool = Arc::clone(&tool);
        tokio::spawn(async move {
            tool.execute(
                ToolCtx::new("c1").with_agent_identity(AgentIdentity {
                    subagent_type: Some("explore".into()),
                    depth: 1,
                    parent_session_id: Some("parent".into()),
                }),
                serde_json::json!({"command": "cargo build"}),
            )
            .await
        })
    };
    sleep(Duration::from_millis(50)).await;
    assert!(
        launcher.take().is_empty(),
        "a child's command must not be handed to the session while it runs"
    );

    release.notify_one();
    let out = running
        .await
        .expect("the call finishes")
        .expect("the call answers");
    assert!(
        out.get("background_task").is_none(),
        "a child's command must not be handed to the session: {out}"
    );
}

struct RecordingWallSession {
    release: Arc<Notify>,
    seen_wall_ms: Mutex<Vec<u64>>,
}

impl SandboxSession for RecordingWallSession {
    fn exec_with_config(
        &self,
        _command: &str,
        config: ExecConfig,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        self.seen_wall_ms
            .lock()
            .expect("wall lock")
            .push(config.wall_timeout_ms);
        let release = Arc::clone(&self.release);
        Box::pin(async move {
            release.notified().await;
            Ok(ExecResult {
                stdout: "pushed\n".into(),
                stderr: String::new(),
                exit_code: Some(0),
            })
        })
    }

    fn workspace_root(&self) -> Arc<Path> {
        Arc::from(temp_dir())
    }
}

/// Explicit run_in_background hands the command over immediately without
/// waiting for the foreground budget, gives the sandbox the extended
/// background ceiling rather than a short caller guess, and rejects admission
/// failures before starting any subprocess.
#[tokio::test]
async fn test_explicit_background_immediate() {
    let release = Arc::new(Notify::new());
    let session = Arc::new(RecordingWallSession {
        release: Arc::clone(&release),
        seen_wall_ms: Mutex::new(Vec::new()),
    });
    let launcher = Arc::new(RecordingLauncher::default());
    let tool = BashTool::new(session.clone()).with_background_commands(
        Arc::clone(&launcher) as Arc<dyn BackgroundCommandLauncher>,
        Duration::from_secs(60),
    );

    let out = tool
        .execute(
            ToolCtx::new("c1"),
            serde_json::json!({
                "command": "git push origin dev",
                "timeout_ms": 400_000,
                "run_in_background": true
            }),
        )
        .await
        .expect("explicit background answers immediately");
    assert_eq!(out["background_task"], serde_json::json!(1));
    assert_eq!(
        *session.seen_wall_ms.lock().expect("wall lock"),
        vec![3_600_000],
        "backgrounded commands run under the extended background ceiling"
    );
    let mut launched = launcher.take();
    assert_eq!(launched.len(), 1);
    release.notify_one();
    assert!(
        launched
            .pop()
            .expect("detached")
            .completion
            .await
            .is_success()
    );

    launcher.refuse_with(BackgroundRefusal::DuplicateCommand {
        existing: RunningTaskSummary {
            id: BackgroundTaskId(1),
            elapsed_secs: 20,
            command: "git push origin dev".into(),
        },
    });
    let err = tool
        .execute(
            ToolCtx::new("c2"),
            serde_json::json!({"command": "git push origin dev", "run_in_background": true}),
        )
        .await
        .expect_err("duplicate explicit background call must fail admission");
    assert!(
        err.to_string().contains("already running"),
        "unexpected error: {err}"
    );
    assert_eq!(
        session.seen_wall_ms.lock().expect("wall lock").len(),
        1,
        "admission refusal must not start a subprocess"
    );
}

/// When all background slots are occupied at the foreground budget, the tool
/// keeps polling the live command in the foreground instead of dropping it or
/// breaching the slot cap.
#[tokio::test]
async fn test_full_slots_keep_foreground() {
    let (session, release) = slow_session();
    let launcher = Arc::new(RecordingLauncher::default());
    launcher.refuse_with(BackgroundRefusal::CapacityFull {
        limit: 4,
        running: Vec::new(),
    });
    let tool = Arc::new(BashTool::new(session).with_background_commands(
        Arc::clone(&launcher) as Arc<dyn BackgroundCommandLauncher>,
        Duration::from_millis(10),
    ));

    let handle = {
        let tool = Arc::clone(&tool);
        tokio::spawn(async move {
            tool.execute(
                ToolCtx::new("c1"),
                serde_json::json!({"command": "cargo test"}),
            )
            .await
        })
    };
    sleep(Duration::from_millis(35)).await;
    release.notify_one();
    let out = handle
        .await
        .expect("task joins")
        .expect("command finishes in foreground");
    assert_eq!(out["exit_code"], serde_json::json!(0));
    assert!(out.get("background_task").is_none());
}

/// When all background slots remain occupied past the foreground fallback
/// ceiling, the tool stops the command and returns a timed-out result rather
/// than waiting forever.
#[tokio::test]
async fn test_full_slots_timeout_stops() {
    let (session, _release) = slow_session();
    let launcher = Arc::new(RecordingLauncher::default());
    launcher.refuse_with(BackgroundRefusal::CapacityFull {
        limit: 4,
        running: Vec::new(),
    });
    let tool = BashTool::new(session).with_background_commands(
        Arc::clone(&launcher) as Arc<dyn BackgroundCommandLauncher>,
        Duration::from_millis(10),
    );
    let out = tool
        .execute(
            ToolCtx::new("c1"),
            serde_json::json!({"command": "cargo test", "timeout_ms": 30}),
        )
        .await
        .expect("fallback ceiling returns timed-out result");
    assert_eq!(out["timed_out"], serde_json::json!(true));
    assert!(
        out["stderr"]
            .as_str()
            .is_some_and(|s| s.contains("background slots were full")),
        "unexpected stderr: {out}"
    );
}
