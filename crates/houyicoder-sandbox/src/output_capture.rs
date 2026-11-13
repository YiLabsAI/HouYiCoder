//! Wire and drain a sandbox shell's three streams.
//!
//! Every backend runs a shell non-interactively: stdin at end of file, both
//! output streams piped and read while the command runs. Reading rather than
//! waiting for exit is what lets a stop report what the command had printed:
//! the bytes land in the caller's carrier as they arrive, so dropping the read
//! future on a wall-clock stop loses nothing already read.

use houyicoder_api::capture::{CaptureVerdict, CapturedStream, RawOutputReceiver};
use houyicoder_context::SandboxError;
use std::future::Future;
use std::io::{Error, Result};
use std::pin::Pin;
use std::process::{ExitStatus, Output};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::sync::Notify;
use tokio::time::timeout;

#[cfg(not(target_os = "windows"))]
use {
    houyicoder_api::launcher::{StdioConfig, StdioMode},
    std::process::Stdio,
    tokio::process::{Child, Command},
};

/// How long the pipes keep draining once the direct child has exited. A
/// descendant that inherited the write ends holds them open for as long as it
/// runs, so the window lets the bytes already in flight land and then stops
/// instead of waiting out the descendant.
pub(crate) const DRAIN_GRACE: Duration = Duration::from_millis(250);

/// The bytes a command has printed so far, owned by the caller. A wall-clock
/// timeout drops the read future, so bytes kept only inside it would be lost
/// exactly when they are needed to explain what the command did.
#[derive(Default)]
pub(crate) struct CapturedOutput {
    stdout: Mutex<Vec<u8>>,
    stderr: Mutex<Vec<u8>>,
}

impl CapturedOutput {
    /// Everything read from stdout, in order. Bytes that are not valid text
    /// become the replacement character rather than failing the read.
    pub(crate) fn stdout_text(&self) -> String {
        let bytes = self.stdout.lock().unwrap_or_else(|e| e.into_inner());
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// Everything read from stderr, in order.
    pub(crate) fn stderr_text(&self) -> String {
        let bytes = self.stderr.lock().unwrap_or_else(|e| e.into_inner());
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// The answer a wall-clock stop becomes, carrying the streams read so
    /// far. They are what tells the caller whether the command was working.
    pub(crate) fn timeout_error(&self, message: String) -> SandboxError {
        SandboxError::Timeout {
            message,
            stdout: self.stdout_text(),
            stderr: self.stderr_text(),
        }
    }

    fn push_stdout(&self, bytes: &[u8]) {
        push(&self.stdout, bytes);
    }

    fn push_stderr(&self, bytes: &[u8]) {
        push(&self.stderr, bytes);
    }

    fn take_stdout(&self) -> Vec<u8> {
        take(&self.stdout)
    }

    fn take_stderr(&self) -> Vec<u8> {
        take(&self.stderr)
    }
}

/// The stream disposition of a sandbox shell, in the launcher's vocabulary:
/// both output streams piped for the drain, stdin at end of file. A shell call
/// is not interactive, so a command that reads stdin (cat, a reader outside a
/// heredoc) finishes at once instead of waiting on the terminal the host is
/// drawing into.
#[cfg(not(target_os = "windows"))]
fn shell_streams() -> StdioConfig {
    StdioConfig::piped_output()
}

/// Point a spawn at the sandbox shell's stream disposition.
#[cfg(not(target_os = "windows"))]
pub(crate) fn pipe_streams(cmd: &mut Command) {
    let streams = shell_streams();
    cmd.stdin(stdio(streams.stdin))
        .stdout(stdio(streams.stdout))
        .stderr(stdio(streams.stderr));
}

#[cfg(not(target_os = "windows"))]
fn stdio(mode: StdioMode) -> Stdio {
    match mode {
        StdioMode::Inherit => Stdio::inherit(),
        StdioMode::Piped => Stdio::piped(),
        StdioMode::Null => Stdio::null(),
    }
}

fn push(buffer: &Mutex<Vec<u8>>, bytes: &[u8]) {
    buffer
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .extend_from_slice(bytes);
}

fn take(buffer: &Mutex<Vec<u8>>) -> Vec<u8> {
    std::mem::take(&mut *buffer.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Read a child's stdout and stderr to the end of its output, returning the
/// same Output shape wait_with_output does. A counter, when given, receives
/// the running stdout newline tally so the host's bash chip can show
/// "(12s · 14 lines)". Both pipes drain concurrently via tokio::join! —
/// reading stdout to EOF first would deadlock if the child fills stderr's
/// 64KB pipe buffer before stdout closes. A command with no stdout never
/// touches the counter (the caller's -1 sentinel stays, so the host shows no
/// line count for it).
///
/// Reading is bounded by the child, not only by the pipes: once the child
/// exits, the reads get the given window (grace) and then stop. A background
/// subshell inherits the write ends, and without the window a command that
/// starts one never looks finished, however quickly the shell itself returns.
pub(crate) trait CaptureChild {
    /// Wait for the child to exit, answering its status. The future is boxed so
    /// it is unpin and can sit behind the select the capture polls.
    fn wait(&mut self) -> Pin<Box<dyn Future<Output = Result<ExitStatus>> + Send + '_>>;
    /// End the child now, without waiting.
    fn start_kill(&mut self) -> Result<()>;
}

#[cfg(not(target_os = "windows"))]
impl CaptureChild for Child {
    fn wait(&mut self) -> Pin<Box<dyn Future<Output = Result<ExitStatus>> + Send + '_>> {
        Box::pin(Child::wait(self))
    }
    fn start_kill(&mut self) -> Result<()> {
        Child::start_kill(self)
    }
}

/// Capture a tokio Child's output through the shared pipeline. The child's
/// stdout and stderr are taken out and the child is handed to the generic
/// pipeline, which drains both streams while it waits and reports what the
/// command printed before a stop or an exit.
#[cfg(not(target_os = "windows"))]
pub(crate) async fn capture(
    mut child: Child,
    lines: Option<&AtomicI64>,
    captured: &CapturedOutput,
    grace: Duration,
    receiver: Option<&dyn RawOutputReceiver>,
) -> Result<Output> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::other("child stdout is not piped"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| Error::other("child stderr is not piped"))?;
    capture_streams(&mut child, stdout, stderr, lines, captured, grace, receiver).await
}

/// Drain two output streams while a child runs, reporting what it printed
/// before a stop or an exit.
///
/// Reading is bounded by the child, not only by the pipes: once the child
/// exits, the reads get the given window (grace) and then stop. A background
/// subshell inherits the write ends, and without the window a command that
/// starts one never looks finished, however quickly the shell itself returns.
pub(crate) async fn capture_streams(
    child: &mut impl CaptureChild,
    mut stdout: impl AsyncRead + Unpin,
    mut stderr: impl AsyncRead + Unpin,
    lines: Option<&AtomicI64>,
    captured: &CapturedOutput,
    grace: Duration,
    receiver: Option<&dyn RawOutputReceiver>,
) -> Result<Output> {
    // Raised when the capture refuses the next chunk. The command then has to
    // stop: its output is going nowhere it can be kept, and a reader that
    // keeps draining would let a runaway command run to its wall budget while
    // the receiver grows or fails.
    let stopped = AtomicBool::new(false);
    let stop_signal = Notify::new();
    let deliver = |stream: CapturedStream, bytes: &[u8]| {
        if let Some(receiver) = receiver
            && matches!(receiver.receive(stream, bytes), CaptureVerdict::Stop(_))
        {
            stopped.store(true, Ordering::Relaxed);
            stop_signal.notify_one();
        }
    };
    let read_stdout = async {
        let mut chunk = vec![0u8; 8192];
        let mut nl: i64 = 0;
        loop {
            if stopped.load(Ordering::Relaxed) {
                break;
            }
            let n = stdout.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            nl += chunk[..n].iter().filter(|b| **b == b'\n').count() as i64;
            if let Some(lines) = lines {
                lines.store(nl, Ordering::Relaxed);
            }
            captured.push_stdout(&chunk[..n]);
            deliver(CapturedStream::Stdout, &chunk[..n]);
        }
        Ok::<_, Error>(())
    };
    let read_stderr = async {
        let mut chunk = vec![0u8; 8192];
        loop {
            if stopped.load(Ordering::Relaxed) {
                break;
            }
            let n = stderr.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            captured.push_stderr(&chunk[..n]);
            deliver(CapturedStream::Stderr, &chunk[..n]);
        }
        Ok::<_, Error>(())
    };
    let reads = async {
        let (out, err) = tokio::join!(read_stdout, read_stderr);
        out?;
        err
    };
    let mut reads = Box::pin(reads);
    let mut wait = child.wait();
    let mut read_result = None;
    let mut status = None;
    let mut stop_early = false;
    tokio::select! {
        waited = &mut wait => status = Some(waited?),
        result = &mut reads => read_result = Some(result),
        _ = stop_signal.notified() => stop_early = true,
    }
    // A refused chunk and a finished read can land together: one stream
    // closes while the other is the one that refused bytes. The stop is
    // therefore read again here, so a command whose output nobody keeps is
    // ended in either order instead of running to its wall budget.
    let status = match status {
        Some(status) => status,
        None if stop_early || stopped.load(Ordering::Relaxed) => {
            // Ending the command is what stops a producer whose output nobody
            // keeps; the call answers with what the buffers hold. A read error
            // that landed beside the stop is dropped with the read futures:
            // the kill here is this call's own decision, and the stop is what
            // the caller has to learn about.
            drop(wait);
            drop(reads);
            if let Err(error) = child.start_kill() {
                tracing::warn!(%error, "sandbox: the capture stop could not end the command");
            }
            return Ok(Output {
                stdout: captured.take_stdout(),
                stderr: captured.take_stderr(),
                status: child.wait().await?,
            });
        }
        None => wait.await?,
    };
    match read_result {
        Some(result) => result?,
        None => match timeout(grace, &mut reads).await {
            Ok(result) => result?,
            // The child is gone while something descended from it still holds
            // the write ends. Whatever it prints from here is not this call's
            // output, so the stop is deliberate; it is recorded because the
            // bytes already buffered are all the caller will ever see.
            Err(_elapsed) => tracing::debug!(
                "sandbox: stream window closed with the child gone; a descendant still holds the pipe"
            ),
        },
    }
    Ok(Output {
        stdout: captured.take_stdout(),
        stderr: captured.take_stderr(),
        status,
    })
}

#[cfg(all(test, unix))]
mod tests;
