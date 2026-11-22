//! BashTool: run a shell command in the sandbox. Destructive commands
//! snapshot the writable roots first, so the command's writes can be reverted.
//!
//! The output is bounded so a runaway command cannot overflow the model
//! context. Past the cap the model reads the tail and a line naming where the
//! rest is: the capture's blocks, or a workspace file when no capture holds
//! them.

use std::sync::{Arc, Mutex};

use houyicoder_api::background::{
    BackgroundCommandLauncher, CommandCompletion, CommandTail, DetachedCommand, TailingReceiver,
};
use houyicoder_api::capture::{RawOutputReceiver, ResultCapture};
use houyicoder_api::sandbox::SandboxSession;
use houyicoder_api::shell::ShellCommand;
use houyicoder_async::{CancellationToken, PFut};
use houyicoder_context::{ExecConfig, ExecResult, SandboxError};
use houyicoder_platform::host::text::OutputEncoding;
use serde_json::{Value, json};

use houyicoder_api::tool::progress::{ToolProgressReporter, ToolProgressUpdate};
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_protocol::extension::ToolError;

use super::bash_snapshot;
use crate::agent::fallback::FallbackToolOutcome;

mod budget;
mod output;
use crate::snapshot::SnapshotStore;
use crate::snapshot::UndoStack;
use crate::snapshot::is_destructive_command;
#[cfg(test)]
use budget::wall_budget_ms;
use budget::{HandoverOutcome, handover_or_wait, resolve_run_budgets};
use output::{complete_run, completion_output};
use std::future::{Future, pending};
use std::sync::atomic::AtomicI64;
use std::time::Duration;
use std::time::Instant;
use tokio::task::spawn_blocking;
use tokio::time::sleep;
use tokio::time::sleep_until;

/// After a failed sandboxed command, scan the session's deny log for
/// authorizable mach-service candidates so the caller can surface or
/// grant them. Skipped on success or when stderr lacks a heuristic
/// sandbox-denial signature (not permitted / denied / sandbox). The
/// heuristic has false negatives (some mach-lookup failures surface as
/// "Connection invalid") and false positives (ordinary file Permission
/// denied); both are acceptable — a missed scan only means missed
/// discovery, and a wasted scan costs one subprocess. Dispatched via
/// spawn_blocking because the scan runs a synchronous subprocess.
async fn discover_authorizable_if_failed(
    success: bool,
    stderr: &str,
    session: Arc<dyn SandboxSession>,
) -> Vec<String> {
    if success || !looks_like_sandbox_denial(stderr) {
        Vec::new()
    } else {
        spawn_blocking(move || session.discover_authorizable())
            .await
            .unwrap_or_default()
    }
}

fn looks_like_sandbox_denial(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("not permitted") || s.contains("denied") || s.contains("sandbox")
}

/// The largest wall-clock budget one call may declare. A build or test run
/// that needs longer than this is a task the caller should split, not a call
/// the host should wait out.
const MAX_TIMEOUT_MS: u64 = 600_000;

/// How long a command may hold the session before the host takes it over.
#[derive(Clone)]
struct BackgroundPolicy {
    launcher: Arc<dyn BackgroundCommandLauncher>,
    budget: Duration,
}

/// Run a shell command in the sandbox. When wired with undo, destructive
/// commands snapshot the workspace before executing so their writes can be
/// reverted.
pub struct BashTool {
    session: Arc<dyn SandboxSession>,
    undo_stack: Option<Arc<Mutex<UndoStack>>>,
    snapshot_store: Option<Arc<SnapshotStore>>,
    active_skill: Option<Arc<Mutex<Option<String>>>>,
    background: Option<BackgroundPolicy>,
}

impl BashTool {
    pub fn new(session: Arc<dyn SandboxSession>) -> Self {
        Self {
            session,
            undo_stack: None,
            snapshot_store: None,
            active_skill: None,
            background: None,
        }
    }

    /// Wire the undo hook: snapshot before destructive exec, push to undo stack.
    pub fn with_undo(
        session: Arc<dyn SandboxSession>,
        undo_stack: Arc<Mutex<UndoStack>>,
        snapshot_store: Arc<SnapshotStore>,
    ) -> Self {
        Self {
            session,
            undo_stack: Some(undo_stack),
            snapshot_store: Some(snapshot_store),
            active_skill: None,
            background: None,
        }
    }

    /// Wire the shared active-skill cell. Deny-log discovery is skipped
    /// when no skill is active because no approval can consume the result.
    pub fn with_active_skill(mut self, cell: Option<Arc<Mutex<Option<String>>>>) -> Self {
        self.active_skill = cell;
        self
    }

    /// Pass commands that outlive their foreground budget to the host's task
    /// registry. The call then answers at once and the command's ending
    /// arrives as a task notification on the next turn. A zero budget keeps
    /// every command in the foreground.
    pub fn with_background_commands(
        mut self,
        launcher: Arc<dyn BackgroundCommandLauncher>,
        budget: Duration,
    ) -> Self {
        self.background = (!budget.is_zero()).then(|| BackgroundPolicy { launcher, budget });
        self
    }

    /// Whether a failed command's deny log should be scanned for services the
    /// user could approve. Without an active skill nothing can consume the
    /// answer, so the scan is skipped.
    fn has_active_skill(&self) -> bool {
        self.active_skill
            .as_ref()
            .is_some_and(|cell| cell.lock().expect("active_skill lock").as_ref().is_some())
    }
}

impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "Run a shell command in the sandbox workspace. \
         Input: {command: string, timeout_ms?: number, run_in_background?: boolean}. \
         Returns stdout/stderr/exit_code; a call stopped at its budget returns \
         timed_out and what the command printed before the stop. \
         Use this for builds, tests, git; a source rewrite from here is refused. \
         Send an edit and the command that checks it in one response, edit \
         first: the check runs as soon as the edit completes. A check after an \
         edit that has not completed is skipped. \
         When issuing multiple commands: if they are independent, make \
         multiple Bash calls in a single message: they run one after another, \
         with no model round trip between them (example: run git status and \
         git diff as two calls in one message, not one compound call). If the \
         commands depend on each other, use a single call with && to chain \
         them. Use ; only when running \
         sequentially without caring if earlier commands fail. Pipe output \
         with | to pass it between commands (find . -name '*.rs' | xargs \
         wc -l | sort -n). Do not use newlines to separate commands. \
         For temporary files and directories use $TMPDIR, which points to the \
         session's own writable temporary directory. /tmp is writable too but \
         shared with other processes on the host, so use it only where a tool \
         hard-codes it. \
         Each foreground call has a wall-clock budget of 120 seconds by default \
         (up to 600000 via timeout_ms). Set run_in_background to true for known \
         multi-minute commands such as full test suites or remote pushes, or let \
         a command crossing the foreground budget move to the background \
         automatically; the call answers with the task number and the ending \
         arrives as a task notification, so do not poll it with sleep. Output \
         over 30 KB is cut to its tail: the note above it names where the rest \
         was kept, and conversation_search reads it back in windows."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "timeout_ms": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_TIMEOUT_MS,
                    "description": "Foreground wall-clock budget in milliseconds. Omit for the default; set run_in_background for known multi-minute commands instead of polling with sleep."
                },
                "run_in_background": {
                    "type": "boolean",
                    "description": "Start this command in the background immediately and receive a task notification when it finishes."
                }
            },
            "required": ["command"]
        })
    }
    fn execute(&self, ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let session = self.session.clone();
        let undo_stack = self.undo_stack.clone();
        let snapshot_store = self.snapshot_store.clone();
        let background = self.background.clone();
        let has_skill = self.has_active_skill();
        let progress = ctx.progress.clone();
        Box::pin(async move {
            let cmd = input
                .get("command")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::InvalidInput("bash: command (string) required".into()))?
                .to_string();
            let in_child = ctx
                .agent_identity
                .as_ref()
                .is_some_and(|identity| identity.depth > 0);
            let budgets = resolve_run_budgets(
                &input,
                in_child,
                background.as_ref().map(|policy| policy.budget),
            );
            if budgets.explicit_background
                && let Some(policy) = background.as_ref()
            {
                policy
                    .launcher
                    .check_admission(&cmd)
                    .map_err(|refusal| ToolError::Failed(format!("bash: {refusal}")))?;
            }
            let notice = match (&snapshot_store, &undo_stack) {
                (Some(store), Some(stack)) if is_destructive_command(&cmd) => {
                    bash_snapshot::prepare(store, stack, &*session)?
                }
                _ => None,
            };
            let started_at = Instant::now();
            let capture = begin_capture(&ctx, session.output_encoding());
            let (mut exec, tail, tail_receiver) = start_captured_exec(
                Arc::clone(&session),
                cmd.clone(),
                budgets.exec_wall_ms,
                &capture,
            );
            let call_id = ctx.call_id.clone();
            let cancel = ctx.cancel.clone();
            let pick = wait_for_run(
                &mut exec,
                cancel.as_ref(),
                budgets.handover_budget,
                progress.as_ref(),
                &tail,
                started_at,
                budgets.fallback_foreground_ms,
            )
            .await;
            let completion = match pick {
                RunPick::Cancelled => return Ok(FallbackToolOutcome::Interrupted.to_json()),
                RunPick::Budget => {
                    let Some(policy) = background.as_ref() else {
                        return Err(ToolError::Failed(
                            "bash: foreground budget without a background host".into(),
                        ));
                    };
                    let detached = build_detached(
                        cmd,
                        started_at,
                        tail,
                        tail_receiver,
                        exec,
                        capture,
                        notice,
                        call_id,
                    );
                    match handover_or_wait(detached, policy, &budgets, cancel.as_ref()).await? {
                        HandoverOutcome::Backgrounded(out) => return Ok(out),
                        HandoverOutcome::Interrupted => {
                            return Ok(FallbackToolOutcome::Interrupted.to_json());
                        }
                        HandoverOutcome::Completed(done) => done,
                    }
                }
                RunPick::Done(outcome) => {
                    complete_run(
                        outcome,
                        capture,
                        tail_receiver.fallback_stop(),
                        notice,
                        call_id,
                    )
                    .await
                }
            };
            format_foreground_result(completion, has_skill, session).await
        })
    }
    fn is_destructive(&self) -> bool {
        true
    }
    /// Begin the capture for this call, recording the encoding the command's
    /// interpreter declared so a later reader decodes the kept bytes under it.
    fn is_read_only(&self) -> bool {
        false
    }
    fn requires_approval(&self) -> bool {
        true
    }
    /// The command the session's shell will actually read. A Git Bash host
    /// runs a POSIX shell, so a command that names the command interpreter's
    /// null device would write a file named nul instead of discarding the
    /// stream; the translated text is what the gate judges and what runs.
    fn resolved_input(&self, input: &Value) -> Option<Value> {
        let command = input.get("command")?.as_str()?;
        let translated = ShellCommand::for_shell(command, self.session.shell());
        if !translated.changed() {
            return None;
        }
        let mut effective = input.clone();
        let object = effective.as_object_mut()?;
        object.insert(
            "command".to_string(),
            Value::String(translated.into_actual()),
        );
        Some(effective)
    }
}

/// Begin the capture for this call, recording the encoding the command's
/// interpreter declared so a later reader decodes the kept bytes under it.
fn begin_capture(ctx: &ToolCtx, encoding: OutputEncoding) -> Option<Arc<dyn ResultCapture>> {
    let source = ctx.result_capture.as_ref()?;
    let session = ctx.session_id?;
    source.begin(session, &ctx.call_id, "bash", encoding)
}

fn start_captured_exec(
    session: Arc<dyn SandboxSession>,
    command: String,
    wall_timeout_ms: u64,
    capture: &Option<Arc<dyn ResultCapture>>,
) -> (
    PFut<'static, Result<ExecResult, SandboxError>>,
    CommandTail,
    Arc<TailingReceiver>,
) {
    let lines_counter = Arc::new(AtomicI64::new(-1));
    let exec_config = ExecConfig {
        wall_timeout_ms,
        ..ExecConfig::default()
    };
    let tail = CommandTail::with_encoding(session.output_encoding());
    let tail_receiver = Arc::new(TailingReceiver::new(
        tail.clone(),
        capture
            .clone()
            .map(|held| held as Arc<dyn RawOutputReceiver>),
    ));
    let receiver: Option<Arc<dyn RawOutputReceiver>> =
        Some(Arc::clone(&tail_receiver) as Arc<dyn RawOutputReceiver>);
    let exec: PFut<'static, Result<ExecResult, SandboxError>> = Box::pin(async move {
        session
            .exec_streaming_captured(&command, lines_counter, exec_config, receiver)
            .await
    });
    (exec, tail, tail_receiver)
}

#[expect(
    clippy::too_many_arguments,
    reason = "the inputs are the run as frozen at launch"
)]
fn build_detached(
    command: String,
    started_at: Instant,
    tail: CommandTail,
    tail_receiver: Arc<TailingReceiver>,
    exec: PFut<'static, Result<ExecResult, SandboxError>>,
    capture: Option<Arc<dyn ResultCapture>>,
    notice: Option<String>,
    call_id: String,
) -> DetachedCommand {
    DetachedCommand {
        command,
        started_at,
        tail,
        completion: Box::pin(async move {
            let outcome = exec.await;
            complete_run(
                outcome,
                capture,
                tail_receiver.fallback_stop(),
                notice,
                call_id,
            )
            .await
        }),
    }
}

async fn format_foreground_result(
    completion: CommandCompletion,
    has_skill: bool,
    session: Arc<dyn SandboxSession>,
) -> Result<Value, ToolError> {
    if let Some(error) = completion.error {
        return Err(ToolError::Failed(error));
    }
    if completion.timed_out {
        return Ok(completion_output(completion, Vec::new()));
    }
    let authorizable = if has_skill && !completion.is_success() {
        discover_authorizable_if_failed(false, &completion.stderr, session).await
    } else {
        Vec::new()
    };
    Ok(completion_output(completion, authorizable))
}

/// Wait for a command to end, to be cancelled, or to reach the foreground
/// budget, reporting its live tail every second meanwhile.
///
/// The exec future is borrowed, not moved: after the budget branch returns,
/// the caller still holds it and passes it to the background host. Dropping it
/// is the cancel path, where the sandbox's RAII kill ends the process group
/// and the tool reports an interrupted outcome so the model log stays
/// lossless.
async fn wait_for_run(
    exec: &mut PFut<'static, Result<ExecResult, SandboxError>>,
    cancel: Option<&CancellationToken>,
    budget: Option<Duration>,
    progress: Option<&Arc<dyn ToolProgressReporter>>,
    tail: &CommandTail,
    started: Instant,
    timeout_ms: u64,
) -> RunPick {
    let deadline = budget.map(|budget| started + budget);
    // The timer is built when this future is first polled, inside the select
    // branch that waits on it: a branch expression is evaluated even when its
    // precondition disables it, and a timer built outside a runtime panics
    // before the run starts.
    let deadline = async move {
        match deadline {
            Some(at) => sleep_until(at.into()).await,
            None => pending().await,
        }
    };
    wait_for_outcome(exec, cancel, deadline, progress, tail, started, timeout_ms).await
}

/// The wait policy: how the three ways a foreground run can end rank against
/// each other. The deadline is a future so the ranking between an answer and
/// an elapsed budget can be observed without waiting on a real clock.
async fn wait_for_outcome<D>(
    exec: &mut PFut<'static, Result<ExecResult, SandboxError>>,
    cancel: Option<&CancellationToken>,
    deadline: D,
    progress: Option<&Arc<dyn ToolProgressReporter>>,
    tail: &CommandTail,
    started: Instant,
    timeout_ms: u64,
) -> RunPick
where
    D: Future<Output = ()>,
{
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            // The command's own result is offered first: a command that
            // ended at the instant the budget ran out produced an answer,
            // and moving it to the background would report a finished run
            // as still going.
            biased;
            outcome = &mut *exec => return RunPick::Done(outcome),
            _ = async {
                match cancel {
                    Some(token) => token.cancelled().await,
                    None => pending().await,
                }
            } => return RunPick::Cancelled,
            _ = &mut deadline => return RunPick::Budget,
            // One reporting tick, then the loop offers the real outcomes
            // again. The branch reports and never decides, which is why the
            // wait has a loop at all: a branch that had to return a pick it
            // never produces would need a branch that cannot be reached.
            _ = async {
                match progress {
                    Some(reporter) => {
                        sleep(Duration::from_secs(1)).await;
                        reporter.update(ToolProgressUpdate {
                            elapsed_secs: started.elapsed().as_secs(),
                            timeout_secs: Some(timeout_ms.div_ceil(1000)),
                            tail: tail.snapshot(),
                        });
                    }
                    None => pending().await,
                }
            }, if progress.is_some() => {}
        }
    }
}

/// Which way the wait for a command ended.
enum RunPick {
    /// The command produced its own result.
    Done(Result<ExecResult, SandboxError>),
    /// The run was cancelled from outside.
    Cancelled,
    /// The foreground budget ran out while the command was still going.
    Budget,
}

#[cfg(test)]
mod tests;
