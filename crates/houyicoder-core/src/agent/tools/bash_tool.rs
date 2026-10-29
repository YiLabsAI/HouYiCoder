//! BashTool: run a shell command in the sandbox. Destructive commands
//! snapshot the writable roots first (so the command's writes can be
//! reverted); the output is
//! bounded so a runaway command cannot overflow the model context (the
//! full output spills to a workspace file past the cap). Split from tools.rs so
//! that file stays under the file-size gate.

use std::path::Path;
use std::sync::{Arc, Mutex};

use houyicoder_api::sandbox::SandboxSession;
use houyicoder_api::shell::ShellCommand;
use houyicoder_async::PFut;
use houyicoder_context::{ExecConfig, ExecResult, SandboxError};
use serde_json::{Value, json};

use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_protocol::extension::ToolError;

use super::bash_snapshot;
use crate::agent::fallback::FallbackToolOutcome;
use crate::snapshot::SnapshotStore;
use crate::snapshot::UndoStack;
use crate::snapshot::is_destructive_command;
use std::fs::File;
use std::fs::create_dir_all;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tokio::task::spawn_blocking;
use tokio::time::sleep;

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

/// Run a shell command in the sandbox. When wired with undo, destructive
/// commands snapshot the workspace before executing so their writes can be
/// reverted.
pub struct BashTool {
    session: Arc<dyn SandboxSession>,
    undo_stack: Option<Arc<Mutex<UndoStack>>>,
    snapshot_store: Option<Arc<SnapshotStore>>,
    active_skill: Option<Arc<Mutex<Option<String>>>>,
}

impl BashTool {
    pub fn new(session: Arc<dyn SandboxSession>) -> Self {
        Self {
            session,
            undo_stack: None,
            snapshot_store: None,
            active_skill: None,
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
        }
    }

    /// Wire the shared active-skill cell. Deny-log discovery is skipped
    /// when no skill is active because no approval can consume the result.
    pub fn with_active_skill(mut self, cell: Option<Arc<Mutex<Option<String>>>>) -> Self {
        self.active_skill = cell;
        self
    }

    /// Turn a finished exec into the tool answer: bound the output so a huge
    /// result cannot overflow the model window, fold the call's own notices
    /// into stderr, and offer any authorizable service a failed command
    /// discovered. The full output above the bound is re-readable through the
    /// spill path the marker names.
    async fn tool_result(&self, result: ExecResult, notice: Option<String>) -> Value {
        let exit_code = result.exit_code;
        let success = result.is_success();
        let (stdout, stderr) =
            bound_bash_output(result.stdout, result.stderr, &self.session.workspace_root());
        let stderr = fold_notice(notice, stderr);
        let has_active_skill = self
            .active_skill
            .as_ref()
            .is_some_and(|cell| cell.lock().expect("active_skill lock").as_ref().is_some());
        let authorizable = if has_active_skill {
            discover_authorizable_if_failed(success, &stderr, self.session.clone()).await
        } else {
            Vec::new()
        };
        let mut output = json!({
            "stdout": stdout,
            "stderr": stderr,
            "exit_code": exit_code,
            "success": success,
        });
        if !authorizable.is_empty() {
            output["authorizable_services"] = json!(authorizable);
        }
        output
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
         When issuing multiple commands: if they are independent, make \
         multiple Bash calls in a single message so they run in parallel \
         (example: run git status and git diff as two calls in one message, \
         not one compound call). If the commands depend on each other, use a \
         single call with && to chain them. Use ; only when running \
         sequentially without caring if earlier commands fail. Pipe output \
         with | to pass it between commands (find . -name '*.rs' | xargs \
         wc -l | sort -n). Do not use newlines to separate commands. \
         For temporary files and directories use $TMPDIR, which points to the \
         session's own writable temporary directory. /tmp is writable too but \
         shared with other processes on the host, so use it only where a tool \
         hard-codes it. \
         Each call has a wall-clock budget of 120 seconds by default. A build \
         or test run that legitimately takes longer sets timeout_ms (up to \
         600000) on that call; do not start work in the background and poll it \
         with sleep."
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
        // Clone the progress sink so a spawned ticker can report elapsed
        // seconds to the host while exec runs. None for non-interactive runs
        // (the ticker then no-ops). The sink ticks every ~1s; the host
        // renders (Ns) on the chip after 2s.
        let progress = ctx.progress.clone();
        Box::pin(async move {
            let cmd = input
                .get("command")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidInput("bash: command (string) required".into()))?;
            // Snapshot a destructive command's writable roots before running
            // it, so the write can be reverted. The outcome is split by what
            // it means, not just Ok/Err, so a performance skip (no copy-on-
            // write + workspace over the slow-copy threshold) does not masquer-
            // ade as a safety refusal and a real I/O failure does not slip
            // through as a skip. The gate asked, the user said yes on the
            // premise that undo exists; a snapshot failure removes that
            // premise, so a real failure refuses to run. A decline returns a
            // notice (undo skipped for speed) that is folded into stderr so
            // it reaches the transcript and the user-facing render rather than
            // a process stderr the TUI never shows. See bash_snapshot::prepare.
            let mut notice: Option<String> = None;
            if let (Some(store), Some(stack)) = (&snapshot_store, &undo_stack)
                && is_destructive_command(cmd)
            {
                notice = bash_snapshot::prepare(store, stack, &*session)?;
            }
            // Run the command, ticking progress every second so a
            // long-running command shows it is not stuck. The line counter
            // (-1 sentinel) is shared with the sandbox exec: a streaming
            // backend (mac) updates it as stdout newlines arrive, so the chip
            // renders (Ns · M lines); a non-streaming backend leaves it at
            // -1, so the tick reports lines=None and the chip shows (Ns)
            // only. The tick loop is dropped (cancelled) the moment exec
            // completes — no leak. Only spawned when a progress sink is
            // wired (interactive runs): tests + non-interactive runs have no
            // sink and skip it (no tokio time driver needed). The tick arm
            // never completes on its own (infinite loop), so the unreachable
            // is a guard, not a path.
            let lines_counter = Arc::new(AtomicI64::new(-1));
            let exec_config = ExecConfig {
                wall_timeout_ms: wall_budget_ms(&input),
                ..ExecConfig::default()
            };
            let exec =
                session.exec_streaming_with_config(cmd, Arc::clone(&lines_counter), exec_config);
            // Cancel arm: an Esc aborts the run and must not wait out a
            // long command. Dropping the exec future triggers the sandbox's
            // RAII kill (process-group killpg on macOS; kill_on_drop on
            // Linux/Windows kills the direct child). The tool reports an
            // interrupted outcome so the model log stays lossless.
            let cancel = ctx.cancel.clone();
            let outcome = if progress.is_some() {
                tokio::select! {
                    r = exec => r,
                    _ = async {
                        if let Some(t) = cancel.as_ref() {
                            t.cancelled().await;
                        }
                    }, if cancel.is_some() => {
                        return Ok(FallbackToolOutcome::Interrupted.to_json());
                    }
                    _ = async {
                        let start = Instant::now();
                        loop {
                            sleep(Duration::from_secs(1)).await;
                            if let Some(p) = &progress {
                                let lines = lines_counter.load(Ordering::Relaxed);
                                let lines_opt = if lines >= 0 {
                                    Some(lines as u64)
                                } else {
                                    None
                                };
                                p.progress(start.elapsed().as_secs(), lines_opt);
                            }
                        }
                    } => unreachable!("bash progress tick loop must not return"),
                }
            } else {
                tokio::select! {
                    r = exec => r,
                    _ = async {
                        if let Some(t) = cancel.as_ref() {
                            t.cancelled().await;
                        }
                    }, if cancel.is_some() => {
                        return Ok(FallbackToolOutcome::Interrupted.to_json());
                    }
                }
            };
            // A stop at the wall-clock budget answers the call rather than
            // failing it. It carries what the command printed before it was
            // stopped, because that output is what tells the caller whether
            // the command was making progress, and a tool error teaches the
            // model to send the same call again or to poll with sleep in the
            // background — the loop the budget exists to shorten.
            let result = match outcome {
                Ok(result) => result,
                Err(SandboxError::Timeout {
                    message,
                    stdout,
                    stderr,
                }) => {
                    let stderr = fold_notice(notice, stderr);
                    return Ok(timed_out_output(
                        &message,
                        stdout,
                        stderr,
                        &self.session.workspace_root(),
                    ));
                }
                Err(e) => return Err(ToolError::Failed(format!("bash: {e}"))),
            };
            Ok(self.tool_result(result, notice).await)
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

/// Fold a notice about the call itself into stderr so the user sees it inline
/// with the command's own output. It is not the command's error, but stderr is
/// the channel the render path already shows verbatim.
fn fold_notice(notice: Option<String>, stderr: String) -> String {
    match notice {
        Some(n) if stderr.is_empty() => n,
        Some(n) => format!("{n}\n{stderr}"),
        None => stderr,
    }
}

/// The answer a command stopped at its wall-clock budget becomes. The exit
/// code is absent rather than zero: nothing exited, and a fabricated code
/// would read as a measured result. The notice goes above whatever the bound
/// leaves of stderr, because output that spilled to a file leaves the model
/// looking at an empty stderr and nothing else would say why the call ended.
fn timed_out_output(message: &str, stdout: String, stderr: String, spill_dir: &Path) -> Value {
    let (stdout, stderr) = bound_bash_output(stdout, stderr, spill_dir);
    let stderr = fold_notice(Some(format!("bash: stopped, {message}")), stderr);
    json!({
        "stdout": stdout,
        "stderr": stderr,
        "success": false,
        "timed_out": true,
    })
}

/// Max chars a bash result may enter the model context before it spills to a
/// workspace file. A runaway command (find on a huge tree, a verbose build) can
/// emit megabytes; without a bound the next turn overflows the context
/// window and the provider rejects with HTTP 400, bricking the session.
///
/// Bounds at 30K chars (~7.5K tokens, safe for any model window) since
/// the compaction runtime is not landed yet; a compaction pass would
/// otherwise keep accumulated results under the window. Relax toward 8MB
/// once the compaction runtime lands.
const BASH_MAX_OUTPUT_CHARS: usize = 30_000;

/// Max chars kept in the stub when the output spills to disk. Char-based
/// (not line) so a no-newline megabyte blob still yields a bounded stub; the
/// tail carries the exit/error context the model most needs, and the full
/// output is re-readable via the spill path in the marker.
const BASH_STUB_CHARS: usize = 5_000;

/// Bound a bash result before it enters the model context. Under the cap the
/// output passes through verbatim. Over the cap the full stdout + stderr
/// spill to a workspace file (stderr lines tagged) and the model gets the tail
/// plus a marker naming the spill path so it can re-read the full output.
fn bound_bash_output(stdout: String, stderr: String, spill_dir: &Path) -> (String, String) {
    let combined = stdout.len() + stderr.len();
    if combined <= BASH_MAX_OUTPUT_CHARS {
        return (stdout, stderr);
    }
    let kb = combined / 1024;
    match spill_bash_output(&stdout, &stderr, spill_dir) {
        Ok(path) => {
            let tail = tail_chars(&stdout, BASH_STUB_CHARS);
            let stub = format!(
                "{tail}\n\nOutput truncated ({kb}KB total). Full output saved to: {}\n",
                path.display()
            );
            // stderr is folded into the spill file (tagged); the model gets
            // the stdout stub + marker only.
            (stub, String::new())
        }
        Err(_) => {
            // Spill failed (workspace dir unwritable): fall back to a head+tail
            // in-memory stub so the result still fits the context budget.
            let half = BASH_MAX_OUTPUT_CHARS / 2;
            let head: String = stdout.chars().take(half).collect();
            let tail_start = stdout.chars().count().saturating_sub(half);
            let tail: String = stdout.chars().skip(tail_start).collect();
            (
                format!("{head}\n... [output truncated, {kb}KB total, spill failed] ...\n{tail}"),
                stderr,
            )
        }
    }
}

/// Write the full stdout + stderr to a workspace file, stderr lines prefixed so
/// the spill is self-describing. Returns the spill path for the marker.
fn spill_bash_output(stdout: &str, stderr: &str, spill_dir: &Path) -> std::io::Result<PathBuf> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = spill_dir.join(".houyicoder").join("bash-output");
    create_dir_all(&dir)?;
    let path = dir.join(format!("houyi-bash-{nanos}.log"));
    let mut f = File::create(&path)?;
    f.write_all(stdout.as_bytes())?;
    if !stderr.is_empty() {
        for line in stderr.split('\n') {
            writeln!(f, "[stderr] {line}")?;
        }
    }
    Ok(path)
}

/// The last N chars of a string, in original order. Char-based (not line)
/// so a no-newline megabyte blob still yields a bounded stub.
fn tail_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if count <= n {
        return s.to_string();
    }
    s.chars().skip(count - n).collect()
}

#[cfg(test)]
mod tests;
