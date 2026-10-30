//! BashTool: run a shell command in the sandbox. Destructive commands
//! snapshot the writable roots first, so the command's writes can be reverted.
//!
//! The output is bounded so a runaway command cannot overflow the model
//! context. Past the cap the model reads the tail and a line naming where the
//! rest is: the capture's blocks, or a workspace file when no capture holds
//! them.

use std::path::Path;
use std::sync::{Arc, Mutex};

use houyicoder_api::capture::{
    CallEnd, CaptureStop, CaptureStopReason, CapturedStreams, RawOutputReceiver, ResultCapture,
};
use houyicoder_api::sandbox::SandboxSession;
use houyicoder_api::shell::ShellCommand;
use houyicoder_async::PFut;
use houyicoder_context::{ExecConfig, ExecResult, SandboxError};
use serde_json::{Value, json};

use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_protocol::capture_record::BlockMarker;
use houyicoder_protocol::extension::ToolError;

use super::bash_snapshot;
use super::results::keys::{CAPTURE_KEY, RECORD_KEY, STDERR_KEY, STDOUT_KEY};
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
    /// discovered. What the bound cut off is read back from the capture, so
    /// the answer names the kept stream instead of copying the bytes into the
    /// workspace.
    async fn tool_result(
        &self,
        result: ExecResult,
        notice: Option<String>,
        capture: CaptureOutcome,
        call_id: &str,
    ) -> Value {
        let exit_code = result.exit_code;
        let success = result.is_success();
        let (stdout, stderr) = bound_bash_output(
            result.stdout,
            result.stderr,
            &self.session.workspace_root(),
            capture.stop().map(|stop| stop.reason),
            &kept_streams(&capture, call_id),
        );
        let stderr = fold_notice(capture.notice(), fold_notice(notice, stderr));
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
        if let Some(reference) = capture.reference() {
            output[CAPTURE_KEY] = reference;
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
         with sleep. Output over 30 KB is cut to its tail: the note above it \
         names where the rest was kept, and conversation_search reads it back \
         in windows."
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
        // Clone the progress handle so a background ticker can report elapsed
        // seconds to the host while exec runs. None for non-interactive runs
        // (the ticker then does nothing). The ticker reports every ~1s; the
        // host renders (Ns) on the chip after 2s.
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
            // completes — no leak. It starts only when progress is present
            // (interactive runs): tests and non-interactive runs report
            // nothing and skip it (no tokio time driver needed). The ticking
            // branch never completes on its own (infinite loop), so the
            // unreachable is a guard, not a path.
            let lines_counter = Arc::new(AtomicI64::new(-1));
            let exec_config = ExecConfig {
                wall_timeout_ms: wall_budget_ms(&input),
                ..ExecConfig::default()
            };
            // Start the raw-output capture before the command runs, so the
            // bytes reach it as the pipe delivers them rather than through the
            // bounded result the model receives. When the host supplies no
            // capture source, the exec runs without one and the bounded result
            // is all that exists.
            let capture = match (ctx.result_capture.as_ref(), ctx.session_id) {
                (Some(source), Some(session_id)) => source.begin(session_id, &ctx.call_id, "bash"),
                _ => None,
            };
            let receiver: Option<Arc<dyn RawOutputReceiver>> = capture
                .clone()
                .map(|capture| capture as Arc<dyn RawOutputReceiver>);
            let exec = session.exec_streaming_captured(
                cmd,
                Arc::clone(&lines_counter),
                exec_config,
                receiver,
            );
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
            // An answered call publishes what it captured, including a
            // command stopped at its budget: the output before the stop is
            // exactly what the caller wants to read back. A failed call does
            // not publish, because no result would name the blocks.
            let result = match outcome {
                Ok(result) => result,
                Err(SandboxError::Timeout {
                    message,
                    stdout,
                    stderr,
                }) => {
                    let drops = model_view_loses_bytes(&stdout, &stderr);
                    let capture = finish_capture(capture, CallEnd::WallBudget, drops).await;
                    let stderr = fold_notice(capture.notice(), fold_notice(notice, stderr));
                    return Ok(timed_out_output(
                        &message,
                        stdout,
                        stderr,
                        &self.session.workspace_root(),
                        &capture,
                        &ctx.call_id,
                    ));
                }
                Err(e) => return Err(ToolError::Failed(format!("bash: {e}"))),
            };
            let drops = model_view_loses_bytes(&result.stdout, &result.stderr);
            let end = CallEnd::Exited(result.exit_code);
            let capture = finish_capture(capture, end, drops).await;
            Ok(self
                .tool_result(result, notice, capture, &ctx.call_id)
                .await)
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
/// leaves of stderr, because output that went to the capture leaves the model
/// looking at an empty stderr and nothing else would say why the call ended.
fn timed_out_output(
    message: &str,
    stdout: String,
    stderr: String,
    spill_dir: &Path,
    capture: &CaptureOutcome,
    call_id: &str,
) -> Value {
    let stopped = capture
        .stop()
        .map(|stop| stop.reason)
        .or(Some(CaptureStopReason::WallBudget));
    let (stdout, stderr) = bound_bash_output(
        stdout,
        stderr,
        spill_dir,
        stopped,
        &kept_streams(capture, call_id),
    );
    let stderr = fold_notice(Some(format!("bash: stopped, {message}")), stderr);
    let mut output = json!({
        "stdout": stdout,
        "stderr": stderr,
        "success": false,
        "timed_out": true,
    });
    if let Some(reference) = capture.reference() {
        output[CAPTURE_KEY] = reference;
    }
    output
}

/// What the raw-output capture produced for one call.
enum CaptureOutcome {
    /// Nothing was published: the model view already carried every byte, or
    /// the host supplied no capture source. The bounded result is all that
    /// exists.
    NotKept,
    /// The raw output went to these blocks.
    Published(CapturedStreams),
    /// The capture could not publish. The bounded result is unaffected and
    /// the result says the raw above the bound was not kept. The stop it
    /// applied before failing still travels: the command was ended either
    /// way, and the bounded result must not read as a complete one.
    Failed { stop: Option<CaptureStop> },
}

impl CaptureOutcome {
    /// Set when the capture stopped the command before its output ended.
    fn stop(&self) -> Option<&CaptureStop> {
        match self {
            Self::Published(captured) => captured.stop.as_ref(),
            Self::Failed { stop } => stop.as_ref(),
            Self::NotKept => None,
        }
    }

    /// The line the result carries when the capture stopped the command. The
    /// command was ended rather than finished, and the bounded result reads as
    /// a complete one unless something says so.
    fn notice(&self) -> Option<String> {
        self.stop().map(|stop| {
            format!(
                "bash: the capture budget was reached after {}KB; the command was stopped and the rest of its output was not kept",
                (stop.stdout_bytes + stop.stderr_bytes) / 1024
            )
        })
    }

    /// The object the tool result carries to name the captured blocks. Each
    /// stream address sits in a block marker of its own, so the log's reclaim
    /// pass reaches every stream block from the result that names it.
    fn reference(&self) -> Option<Value> {
        match self {
            Self::NotKept => None,
            Self::Failed { stop } => {
                let mut reference = json!({
                    "capture_failed": "the raw output above the bound was not kept",
                });
                if let Some(stop) = stop {
                    reference["stop_reason"] = json!(stop.reason);
                }
                Some(reference)
            }
            Self::Published(captured) => {
                let mut reference = json!({
                    RECORD_KEY: BlockMarker::new(captured.record.0.clone()),
                    "stdout_bytes": captured.stdout_bytes,
                    "stderr_bytes": captured.stderr_bytes,
                });
                if let Some(hash) = &captured.stdout {
                    reference[STDOUT_KEY] = json!(BlockMarker::new(hash.0.clone()));
                }
                if let Some(hash) = &captured.stderr {
                    reference[STDERR_KEY] = json!(BlockMarker::new(hash.0.clone()));
                }
                if let Some(reason) = captured.stop_reason {
                    reference["stop_reason"] = json!(reason);
                }
                Some(reference)
            }
        }
    }
}

/// Publish what the capture holds, but only when the model view lost bytes to
/// the bound. Below it the model receives the whole output, so a block would
/// be a second copy of what the result already carries.
///
/// A failure to publish is an answer the caller reports, never a silent
/// success: the model still receives the bounded output, and the operator
/// reads the cause in the diagnostic log.
async fn finish_capture(
    capture: Option<Arc<dyn ResultCapture>>,
    end: CallEnd,
    drops: bool,
) -> CaptureOutcome {
    if !drops {
        return CaptureOutcome::NotKept;
    }
    let Some(capture) = capture else {
        return CaptureOutcome::NotKept;
    };
    let stop = capture.stop();
    match capture.finish(end).await {
        Ok(Some(captured)) => CaptureOutcome::Published(captured),
        Ok(None) => CaptureOutcome::NotKept,
        Err(error) => {
            tracing::warn!(%error, "bash: the raw output capture could not publish");
            CaptureOutcome::Failed { stop }
        }
    }
}

/// Max chars a bash result may enter the model context before it is cut down
/// to a tail. A runaway command (find on a huge tree, a verbose build) can
/// emit megabytes; without a bound the next turn overflows the context
/// window and the provider rejects with HTTP 400, bricking the session.
///
/// Bounds at 30K chars (~7.5K tokens, safe for any model window). The kept
/// copy stays in the capture, so raising the bound later costs model context
/// but no stored bytes.
const BASH_MAX_OUTPUT_CHARS: usize = 30_000;

/// Max chars kept in the tail the model sees once the output passes the cap.
/// Char-based (not line) so a no-newline megabyte blob still yields a bounded
/// tail; the tail carries the exit/error context the model most needs, and
/// the note under it names where the rest can be read.
const BASH_STUB_CHARS: usize = 5_000;

/// Whether the model view loses bytes to the bound. One answer serves both the
/// bounding itself and the decision to keep the raw output, so the two cannot
/// disagree about what the model saw.
fn model_view_loses_bytes(stdout: &str, stderr: &str) -> bool {
    stdout.len() + stderr.len() > BASH_MAX_OUTPUT_CHARS
}

/// Bound a bash result before it enters the model context. Under the cap the
/// output passes through verbatim. Over the cap the model gets the tail plus a
/// note saying where the rest is: the covered bytes stay in the capture and
/// the note names that stream, so the full output is read back in bounded
/// windows instead of copied into the workspace. Only when no capture holds
/// the bytes does the rest spill to a workspace file.
///
/// When the command was stopped before it finished, neither copy holds more
/// than it printed before the stop. The note says so: a reader told the full
/// output was saved goes looking for bytes that were never kept.
fn bound_bash_output(
    stdout: String,
    stderr: String,
    spill_dir: &Path,
    stopped: Option<CaptureStopReason>,
    kept: &[String],
) -> (String, String) {
    if !model_view_loses_bytes(&stdout, &stderr) {
        return (stdout, stderr);
    }
    let kb = (stdout.len() + stderr.len()) / 1024;
    if !kept.is_empty() {
        let tail = tail_chars(&stdout, BASH_STUB_CHARS);
        let note = kept_note(kept, stopped);
        return (
            format!("{tail}\n\nOutput truncated ({kb}KB total). {note}\n"),
            String::new(),
        );
    }
    match spill_bash_output(&stdout, &stderr, spill_dir) {
        Ok(path) => {
            let tail = tail_chars(&stdout, BASH_STUB_CHARS);
            let saved = saved_note(&path, kb, stopped);
            let stub = format!("{tail}\n\nOutput truncated ({kb}KB total). {saved}\n");
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
            let why = match stopped {
                Some(reason) => format!("the command was stopped, {reason}"),
                None => format!("{kb}KB total"),
            };
            (
                format!("{head}\n... [output truncated, {why}, spill failed] ...\n{tail}"),
                stderr,
            )
        }
    }
}

/// The line that tells the model where the rest of the output went, and how
/// much of it is really there. A command stopped mid-stream leaves the kept
/// copy holding only a prefix, so a reader told the full output was saved
/// would go looking for bytes that were never kept.
fn saved_note(path: &Path, kb: usize, stopped: Option<CaptureStopReason>) -> String {
    match stopped {
        Some(reason) => format!(
            "the command was stopped, {reason}, so this file holds only the first {kb}KB of its output: {}",
            path.display()
        ),
        None => format!("Full output saved to: {}", path.display()),
    }
}

/// The line that points the model at the streams the capture kept, named the
/// way the recall tool lists them: the call id and the stream. Stderr left the
/// model view with the truncation and rides in the same note, so a reader who
/// only sees this line still knows both streams survived.
fn kept_note(kept: &[String], stopped: Option<CaptureStopReason>) -> String {
    let names = kept.join(" and ");
    match stopped {
        Some(reason) => format!(
            "the command was stopped, {reason}, so only the prefix it printed before the stop was kept, as {names}; read it with conversation_search (results read)"
        ),
        None => format!(
            "The full output is kept as {names}; read it with conversation_search (results read)"
        ),
    }
}

/// The streams the capture actually published, named the way the recall tool
/// lists them. Empty when it published nothing, which includes a capture that
/// failed: nothing is stored then, so the caller keeps another copy of the
/// bytes instead of pointing at streams that do not exist.
fn kept_streams(capture: &CaptureOutcome, call_id: &str) -> Vec<String> {
    match capture {
        CaptureOutcome::Published(captured) => {
            let mut names = Vec::new();
            if captured.stdout.is_some() {
                names.push(format!("{call_id}:{STDOUT_KEY}"));
            }
            if captured.stderr.is_some() {
                names.push(format!("{call_id}:{STDERR_KEY}"));
            }
            names
        }
        CaptureOutcome::NotKept | CaptureOutcome::Failed { .. } => Vec::new(),
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
