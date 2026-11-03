//! Tests for a command that outlives its foreground budget.

use super::super::*;
use houyicoder_api::background::BackgroundCommandLauncher;
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
}

impl RecordingLauncher {
    pub(super) fn take(&self) -> Vec<DetachedCommand> {
        take(&mut *self.launched.lock().expect("launcher lock"))
    }
}

impl BackgroundCommandLauncher for RecordingLauncher {
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
    let tool = BashTool::new(session).with_background_commands(
        Arc::clone(&launcher) as Arc<dyn BackgroundCommandLauncher>,
        Duration::from_millis(10),
    );
    // The command finishes at once, so only the budget could have moved it.
    release.notify_one();

    let out = tool
        .execute(
            ToolCtx::new("c1").with_agent_identity(AgentIdentity {
                subagent_type: Some("explore".into()),
                depth: 1,
                parent_session_id: Some("parent".into()),
            }),
            serde_json::json!({"command": "cargo build"}),
        )
        .await
        .expect("the call answers");

    assert!(
        out.get("background_task").is_none(),
        "a child's command must not be handed to the session: {out}"
    );
    assert!(
        launcher.take().is_empty(),
        "the launcher must never see a child's command"
    );
}
