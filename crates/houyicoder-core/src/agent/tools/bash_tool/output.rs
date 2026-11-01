//! Reduces a finished command to the value the model reads. The bound keeps a
//! runaway command from overflowing the context window, and the note under it
//! says where the rest of the output was kept. Both the foreground answer and
//! the ending delivered at a later turn come through here, so a command that
//! ends inside the foreground budget and one that outlives it report the same
//! way.

use std::sync::Arc;
use std::time::Duration;

use houyicoder_api::background::CommandCompletion;
use houyicoder_api::capture::{
    CallEnd, CaptureStop, CaptureStopReason, CapturedStreams, ResultCapture,
};
use houyicoder_context::{ExecResult, SandboxError};
use houyicoder_protocol::capture_record::{
    BlockMarker, CAPTURE_FAILED_KEY, CAPTURE_KEY, RECORD_KEY, STDERR_BYTES_KEY, STDERR_KEY,
    STDOUT_BYTES_KEY, STDOUT_KEY, STOP_REASON_KEY,
};
use houyicoder_protocol::frontend::BackgroundTaskId;
use serde_json::{Value, json};

/// Fold a notice about the call itself into stderr so the user sees it inline
/// with the command's own output. It is not the command's error, but stderr is
/// the channel the render path already shows verbatim.
pub(super) fn fold_notice(notice: Option<String>, stderr: String) -> String {
    match notice {
        Some(n) if stderr.is_empty() => n,
        Some(n) => format!("{n}\n{stderr}"),
        None => stderr,
    }
}

/// Reduce one finished run to the value both the foreground answer and the
/// detached notification are built from. Bounding, notice folding, and the
/// capture publish happen here once, so a command that ends inside the budget
/// and one that ends after the background move cannot report different endings.
pub(super) async fn complete_run(
    outcome: Result<ExecResult, SandboxError>,
    capture: Option<Arc<dyn ResultCapture>>,
    notice: Option<String>,
    call_id: String,
) -> CommandCompletion {
    match outcome {
        Ok(result) => {
            let drops = model_view_loses_bytes(&result.stdout, &result.stderr);
            let end = CallEnd::Exited(result.exit_code);
            let capture = finish_capture(capture, end, drops).await;
            let (stdout, stderr) = bound_bash_output(
                result.stdout,
                result.stderr,
                capture.stop().map(|stop| stop.reason),
                &kept_streams(&capture, &call_id),
            );
            let stderr = fold_notice(capture.notice(), fold_notice(notice, stderr));
            CommandCompletion {
                exit_code: result.exit_code,
                stdout,
                stderr,
                timed_out: false,
                error: None,
                capture: capture.reference(),
            }
        }
        // A stop at the wall-clock budget is an answer, not an error: the
        // command printed something before it was stopped. The stop notice
        // goes above whatever the bound leaves of stderr, because output that
        // went to the capture leaves the model looking at an empty stderr and
        // nothing else would say why the call ended.
        Err(SandboxError::Timeout {
            message,
            stdout,
            stderr,
        }) => {
            let drops = model_view_loses_bytes(&stdout, &stderr);
            let capture = finish_capture(capture, CallEnd::WallBudget, drops).await;
            let stopped = capture
                .stop()
                .map(|stop| stop.reason)
                .or(Some(CaptureStopReason::WallBudget));
            let stderr = fold_notice(capture.notice(), fold_notice(notice, stderr));
            let (stdout, stderr) =
                bound_bash_output(stdout, stderr, stopped, &kept_streams(&capture, &call_id));
            let stderr = fold_notice(Some(format!("bash: stopped, {message}")), stderr);
            CommandCompletion {
                exit_code: None,
                stdout,
                stderr,
                timed_out: true,
                error: None,
                capture: capture.reference(),
            }
        }
        Err(error) => CommandCompletion {
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            timed_out: false,
            error: Some(format!("bash: {error}")),
            capture: None,
        },
    }
}

/// The foreground answer for a finished run. The exit code is absent on a
/// run stopped at its budget rather than zero: nothing exited, and a
/// fabricated code would read as a measured result.
pub(super) fn completion_output(completion: CommandCompletion, authorizable: Vec<String>) -> Value {
    let mut output = if completion.timed_out {
        json!({
            "stdout": completion.stdout,
            "stderr": completion.stderr,
            "success": false,
            "timed_out": true,
        })
    } else {
        json!({
            "stdout": completion.stdout,
            "stderr": completion.stderr,
            "exit_code": completion.exit_code,
            "success": completion.is_success(),
        })
    };
    if !authorizable.is_empty() {
        output["authorizable_services"] = json!(authorizable);
    }
    if let Some(reference) = completion.capture {
        output[CAPTURE_KEY] = reference;
    }
    output
}

/// The answer a background command leaves the model with. The call did not
/// finish, so it carries no exit status; it names the task number and the
/// running tail, and says the ending will arrive as a task notification.
pub(super) fn background_output(
    task: BackgroundTaskId,
    tail: Vec<String>,
    budget: Duration,
) -> Value {
    json!({
        "stdout": "",
        "stderr": format!(
            "bash: still running after {}s; moved to the background as task {}. Its ending arrives as a task notification at the start of the next turn; do not poll it.",
            budget.as_secs(),
            task.label(),
        ),
        "background_task": task.0,
        "tail": tail,
    })
}

/// What the raw-output capture produced for one call.
pub(super) enum CaptureOutcome {
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
                    CAPTURE_FAILED_KEY: "the raw output above the bound was not kept",
                });
                if let Some(stop) = stop {
                    reference[STOP_REASON_KEY] = json!(stop.reason);
                }
                Some(reference)
            }
            Self::Published(captured) => {
                let mut reference = json!({
                    RECORD_KEY: BlockMarker::new(captured.record.0.clone()),
                    STDOUT_BYTES_KEY: captured.stdout_bytes,
                    STDERR_BYTES_KEY: captured.stderr_bytes,
                });
                if let Some(hash) = &captured.stdout {
                    reference[STDOUT_KEY] = json!(BlockMarker::new(hash.0.clone()));
                }
                if let Some(hash) = &captured.stderr {
                    reference[STDERR_KEY] = json!(BlockMarker::new(hash.0.clone()));
                }
                if let Some(reason) = captured.stop_reason {
                    reference[STOP_REASON_KEY] = json!(reason);
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
pub(super) const BASH_MAX_OUTPUT_CHARS: usize = 30_000;

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
/// note saying where the rest is read back: when the capture kept a stream the
/// note names it and points at conversation_search, so the full output is read
/// in bounded windows instead of copied into the workspace. When no capture
/// holds the bytes, the model gets a head+tail stub and is told the rest was
/// not kept, rather than being sent to a workspace file it would have to
/// re-run a command to read.
///
/// When the command was stopped before it finished, neither copy holds more
/// than it printed before the stop. The note says so: a reader told the full
/// output was saved goes looking for bytes that were never kept.
pub(super) fn bound_bash_output(
    stdout: String,
    stderr: String,
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
    // No capture holds the bytes (a legacy session, or the host supplied no
    // capture source). Keep the result bounded with the stdout tail and say
    // the rest was not kept, so the model is not pointed at a file it would
    // need another command to read. Stderr is bounded to the same tail too:
    // it is usually small, but a command that floods stderr must not blow the
    // context the way an unbounded stdout would.
    let stdout_tail = tail_chars(&stdout, BASH_STUB_CHARS);
    let stderr_tail = tail_chars(&stderr, BASH_STUB_CHARS);
    let why = match stopped {
        Some(reason) => format!("the command was stopped, {reason}"),
        None => format!("{kb}KB total"),
    };
    (
        format!("{stdout_tail}\n\nOutput truncated ({why}); the rest was not kept.\n"),
        stderr_tail,
    )
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

/// The last N chars of a string, in original order. Char-based (not line)
/// so a no-newline megabyte blob still yields a bounded stub.
pub(super) fn tail_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if count <= n {
        return s.to_string();
    }
    s.chars().skip(count - n).collect()
}
