// Test scaffolding runs real shells to exercise the drain, not an engine
// spawn, so the process-chokepoint rule is allowed here.
#![allow(clippy::disallowed_methods)]
use super::{CapturedOutput, DRAIN_GRACE, capture, pipe_streams};
use houyicoder_api::capture::{
    CaptureStop, CaptureStopReason, CaptureVerdict, CapturedStream, RawOutputReceiver,
};
use houyicoder_context::SandboxError;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::time::timeout;

/// A shell that reads stdin finishes at once: the drain sets stdin to end of
/// file, so a pipe the caller handed in cannot hold it open. Asserted with a
/// live writer kept on that pipe, which would keep cat waiting forever if the
/// disposition were left alone.
#[tokio::test]
async fn test_pipe_streams_ends_stdin() {
    let (held_pipe, _writer) = UnixStream::pair().expect("socket pair");
    let read_end: OwnedFd = held_pipe.into();
    let mut cmd = Command::new("cat");
    cmd.stdin(Stdio::from(read_end));
    pipe_streams(&mut cmd);
    let child = cmd.spawn().expect("run cat");
    let captured = CapturedOutput::default();
    let outcome = timeout(
        Duration::from_secs(2),
        capture(child, None, &captured, DRAIN_GRACE, None),
    )
    .await
    .expect("cat must reach end of file instead of waiting on the held pipe");
    let output = outcome.expect("capture");
    assert!(output.stdout.is_empty(), "nothing was written to stdin");
    assert!(output.status.success());
}

/// capture counts stdout newlines live + returns the full output (same shape
/// as wait_with_output). Runs a real shell without sandbox-exec, since the
/// read is backend-agnostic, so it is fast + unix-only.
#[tokio::test]
async fn test_capture_counts_lines() {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg("printf '1\\n2\\n3\\n4\\n5\\n'");
    pipe_streams(&mut cmd);
    let child = cmd.spawn().expect("spawn sh");
    let counter = Arc::new(AtomicI64::new(-1));
    let captured = CapturedOutput::default();
    let output = capture(child, Some(&counter), &captured, DRAIN_GRACE, None)
        .await
        .expect("capture");
    assert_eq!(counter.load(Ordering::Relaxed), 5, "final line count");
    assert_eq!(output.stdout, b"1\n2\n3\n4\n5\n");
    assert!(output.status.success(), "sh exited clean");
}

/// A command with no stdout leaves the counter at the -1 sentinel (the host
/// shows no line count for it); the exit status still lands.
#[tokio::test]
async fn test_capture_keeps_sentinel() {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg("true");
    pipe_streams(&mut cmd);
    let child = cmd.spawn().expect("spawn sh");
    let counter = Arc::new(AtomicI64::new(-1));
    let captured = CapturedOutput::default();
    let output = capture(child, Some(&counter), &captured, DRAIN_GRACE, None)
        .await
        .expect("capture");
    assert_eq!(counter.load(Ordering::Relaxed), -1, "no stdout → sentinel");
    assert!(output.stdout.is_empty());
    assert!(output.status.success());
}

/// Bytes read before a stop stay readable in the carrier after the read
/// future is dropped, and binary output never panics the conversion.
#[test]
fn test_capture_keeps_partial() {
    let captured = CapturedOutput::default();
    captured.push_stdout(b"built\n");
    captured.push_stderr(&[0xff, b'\n']);
    assert_eq!(captured.stdout_text(), "built\n");
    assert!(
        captured.stderr_text().starts_with('\u{fffd}'),
        "invalid bytes become the replacement character: {:?}",
        captured.stderr_text()
    );
    assert_eq!(captured.take_stdout(), b"built\n");
    assert!(
        captured.stdout_text().is_empty(),
        "taking hands the bytes over"
    );
}

/// A stop answers with the streams read so far and nothing invented: the
/// report names the budget, and an empty stream stays empty.
#[test]
fn test_stop_reports_read_bytes() {
    let captured = CapturedOutput::default();
    let err = captured.timeout_error("wall-clock 0.2s exceeded".to_string());
    let SandboxError::Timeout {
        message,
        stdout,
        stderr,
    } = err
    else {
        panic!("a budget stop must be a timeout, got {err:?}");
    };
    assert_eq!(message, "wall-clock 0.2s exceeded");
    assert!(stdout.is_empty(), "nothing was collected");
    assert!(stderr.is_empty(), "nothing was collected");
}

/// Reading a child that was not piped is an error, not an empty result: a
/// backend that skips the stream wiring must not look like a silent command.
#[tokio::test]
async fn test_capture_requires_pipes() {
    let mut cmd = Command::new("true");
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    let child = cmd.spawn().expect("run true");
    let captured = CapturedOutput::default();
    let err = capture(child, None, &captured, DRAIN_GRACE, None)
        .await
        .expect_err("an unpiped child cannot be drained");
    assert!(
        err.to_string().contains("not piped"),
        "the failure names the missing pipe: {err}"
    );
}

/// A background descendant that inherits the pipe must not hold the call
/// open after the shell itself exits: reading stops once the child is gone,
/// so a command that starts a slow job and prints returns at once with what
/// it printed, instead of waiting out the job behind it.
#[tokio::test]
async fn test_read_stops_at_exit() {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg("(sleep 5; echo late) & echo started");
    pipe_streams(&mut cmd);
    let child = cmd.spawn().expect("spawn sh");
    let captured = CapturedOutput::default();
    let started = Instant::now();
    let output = capture(child, None, &captured, Duration::from_millis(10), None)
        .await
        .expect("capture");
    let waited = started.elapsed();
    assert!(
        waited < Duration::from_secs(1),
        "the background holder must not extend the call: waited {waited:?}"
    );
    assert_eq!(
        output.stdout, b"started\n",
        "output printed before exit is kept"
    );
    assert!(output.status.success());
}

/// Refuses the first chunk it is handed.
struct StopsAtOnce;

impl RawOutputReceiver for StopsAtOnce {
    fn receive(&self, _stream: CapturedStream, _bytes: &[u8]) -> CaptureVerdict {
        CaptureVerdict::Stop(CaptureStop {
            reason: CaptureStopReason::ByteBudget,
            stdout_bytes: 0,
            stderr_bytes: 0,
        })
    }
}

/// A capture that refuses the next chunk ends the command instead of draining
/// a producer whose output is going nowhere. Without this, a command printing
/// without end would run until its wall-clock budget.
#[tokio::test]
async fn test_stop_ends_command() {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg("yes");
    pipe_streams(&mut cmd);
    let child = cmd.spawn().expect("spawn sh");
    let captured = CapturedOutput::default();
    let started = Instant::now();
    let output = timeout(
        Duration::from_secs(2),
        capture(child, None, &captured, DRAIN_GRACE, Some(&StopsAtOnce)),
    )
    .await
    .expect("the stop must end the command rather than drain it")
    .expect("capture");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the kill answers the call: waited {:?}",
        started.elapsed()
    );
    assert!(
        !output.status.success(),
        "the producer was killed, not finished"
    );
}

/// A stop that lands while the reads finish must still end the command. With
/// one stream closed at once, its read ends before the other stream refuses
/// bytes, so the finished reads and the stop arrive together; a drain that
/// only watched for the stop signal would then wait for a producer that never
/// stops printing.
#[tokio::test]
async fn test_stop_while_reads_finish() {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg("exec 2>/dev/null; yes");
    pipe_streams(&mut cmd);
    let child = cmd.spawn().expect("spawn sh");
    let captured = CapturedOutput::default();
    let started = Instant::now();
    let output = timeout(
        Duration::from_secs(2),
        capture(child, None, &captured, DRAIN_GRACE, Some(&StopsAtOnce)),
    )
    .await
    .expect("a stop beside finished reads must end the command")
    .expect("capture");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the kill answers the call: waited {:?}",
        started.elapsed()
    );
    assert!(
        !output.status.success(),
        "the producer was killed, not finished"
    );
}
