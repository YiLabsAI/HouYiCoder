//! BashTool: run a shell command in the sandbox. Destructive commands
//! snapshot the writable roots first, so the command's writes can be reverted.
//!
//! The output is bounded so a runaway command cannot overflow the model
//! context. Past the cap the model reads the tail and a line naming where the
//! rest is: the capture's blocks, or a workspace file when no capture holds
//! them.

use std::sync::{Arc, Mutex};

use houyicoder_api::background::{
    BackgroundCommandLauncher, CommandTail, DetachedCommand, TailingReceiver,
};
use houyicoder_api::capture::RawOutputReceiver;
use houyicoder_api::sandbox::SandboxSession;
use houyicoder_api::shell::ShellCommand;
use houyicoder_async::{CancellationToken, PFut};
use houyicoder_context::{ExecConfig, ExecResult, SandboxError};
use serde_json::{Value, json};

use houyicoder_api::tool::progress::{ToolProgressReporter, ToolProgressUpdate};
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_protocol::extension::ToolError;

use super::bash_snapshot;
use crate::agent::fallback::FallbackToolOutcome;

mod output;
use crate::snapshot::SnapshotStore;
use crate::snapshot::UndoStack;
use crate::snapshot::is_destructive_command;
use output::{background_output, complete_run, completion_output};
use std::future::pending;
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

/// The wall-clock budget this call declares, clamped into the range the
/// schema advertises. A missing or malformed value keeps the session default,
/// so a caller that does not think about the budget gets the ordinary one.
/// The shorter name is accepted alongside the documented one because a budget
/// that reads as missing is worse than no budget at all: the caller believes
/// it raised the limit and the command is stopped at the default anyway.
fn wall_budget_ms(input: &Value) -> u64 {
    ["timeout_ms", "timeout"]
        .iter()
        .find_map(|name| input.get(*name).and_then(Value::as_u64))
        .map(|ms| ms.clamp(1, MAX_TIMEOUT_MS))
        .unwrap_or_else(|| ExecConfig::default().wall_timeout_ms)
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
         Input: {command: string, timeout_ms?: number}. \
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
         Each call has a wall-clock budget of 120 seconds by default. A build \
         or test run that legitimately takes longer sets timeout_ms (up to \
         600000) on that call. A command still running when the foreground \
         budget ends is moved to the background automatically; the call \
         answers with the task number and the ending arrives as a task \
         notification, so do not poll it with sleep. Output over 30 KB is cut \
         to its tail: the note above it names where the rest was kept, and \
         conversation_search reads it back in windows."
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
                    "description": "Wall-clock budget for this call in milliseconds. Omit for the default; a slow build or test run raises it instead of polling with sleep."
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
        // The progress handle feeds the tick loop below; non-interactive runs
        // carry none and skip the loop entirely.
        let progress = ctx.progress.clone();
        Box::pin(async move {
            let cmd = input
                .get("command")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::InvalidInput("bash: command (string) required".into()))?
                .to_string();
            // Snapshot a destructive command's writable roots before running
            // it, so the write can be reverted. A performance skip is a notice
            // rather than a refusal; a real I/O failure refuses to run,
            // because the user approved on the premise that undo exists. The
            // notice is folded into stderr so the transcript shows it.
            let mut notice: Option<String> = None;
            if let (Some(store), Some(stack)) = (&snapshot_store, &undo_stack)
                && is_destructive_command(&cmd)
            {
                notice = bash_snapshot::prepare(store, stack, &*session)?;
            }
            let timeout_ms = wall_budget_ms(&input);
            // The sandbox's streaming signature takes a shared stdout line
            // counter. This tool reads its preview from the tail receiver, so
            // nothing here reads the counter back.
            let lines_counter = Arc::new(AtomicI64::new(-1));
            let exec_config = ExecConfig {
                wall_timeout_ms: timeout_ms,
                ..ExecConfig::default()
            };
            // Taken before the command starts: the host measures a task from
            // here, so its duration includes the foreground wait.
            let started_at = Instant::now();
            // The capture and the tail ride the same receiver, so previewing a
            // run costs no bytes from what the host keeps.
            let capture = match (ctx.result_capture.as_ref(), ctx.session_id) {
                (Some(source), Some(session_id)) => source.begin(session_id, &ctx.call_id, "bash"),
                _ => None,
            };
            let tail = CommandTail::new();
            let receiver: Option<Arc<dyn RawOutputReceiver>> =
                Some(Arc::new(TailingReceiver::new(
                    tail.clone(),
                    capture
                        .clone()
                        .map(|held| held as Arc<dyn RawOutputReceiver>),
                )));
            // Boxed to a 'static future so the host can take it over when the
            // foreground budget ends; it owns the session and command.
            let exec_session = Arc::clone(&session);
            let exec_command = cmd.clone();
            let exec_lines = Arc::clone(&lines_counter);
            let mut exec: PFut<'static, Result<ExecResult, SandboxError>> = Box::pin(async move {
                exec_session
                    .exec_streaming_captured(&exec_command, exec_lines, exec_config, receiver)
                    .await
            });
            let cancel = ctx.cancel.clone();
            let budget = background.as_ref().map(|policy| policy.budget);
            let pick = wait_for_run(
                &mut exec,
                cancel.as_ref(),
                budget,
                progress.as_ref(),
                &tail,
                started_at,
                timeout_ms,
            )
            .await;
            let call_id = ctx.call_id.clone();
            let completion = match pick {
                RunPick::Cancelled => return Ok(FallbackToolOutcome::Interrupted.to_json()),
                // The host takes the run over; the detached future never
                // inherits the parent's cancel token, so an Esc on the next
                // turn cannot kill a command the user was told is running.
                RunPick::Budget => {
                    let Some(policy) = background.as_ref() else {
                        return Err(ToolError::Failed(
                            "bash: foreground budget without a background host".into(),
                        ));
                    };
                    let task = policy.launcher.launch(DetachedCommand {
                        command: cmd,
                        started_at,
                        tail: tail.clone(),
                        completion: Box::pin(async move {
                            let outcome = exec.await;
                            complete_run(outcome, capture, notice, call_id).await
                        }),
                    });
                    return Ok(background_output(task, tail.snapshot(), policy.budget));
                }
                RunPick::Done(outcome) => complete_run(outcome, capture, notice, call_id).await,
            };
            if let Some(error) = completion.error {
                return Err(ToolError::Failed(error));
            }
            // A stop at the wall-clock budget answers the call rather than
            // failing it. It carries what the command printed before it was
            // stopped: a tool error would teach the model to send the same
            // call again, the loop the budget exists to shorten.
            if completion.timed_out {
                return Ok(completion_output(completion, Vec::new()));
            }
            let authorizable = if self.has_active_skill() && !completion.is_success() {
                discover_authorizable_if_failed(false, &completion.stderr, self.session.clone())
                    .await
            } else {
                Vec::new()
            };
            Ok(completion_output(completion, authorizable))
        })
    }
    fn is_destructive(&self) -> bool {
        true
    }
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
            // Every timer is built inside its branch: a select branch's
            // expression is evaluated even when its precondition disables it,
            // and a timer built outside a runtime panics before the run starts.
            _ = async {
                match deadline {
                    Some(at) => sleep_until(at.into()).await,
                    None => pending().await,
                }
            } => return RunPick::Budget,
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
