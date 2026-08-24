//! The default launcher implementation: routes every spawn through a std
//! Command (sync output capture) or a tokio Command (async child with
//! kill-on-drop) behind the ProcessLauncher trait. This is the legitimate home
//! for raw Command usage outside the sandbox crate, so the clippy disallowed
//! methods rule is allowed on the two spawn paths below.
//!
//! Policy handling is best-effort for the first cut:
//! - audit: emits a structured spawn log line to stderr.
//! - fence: the wall timeout is enforced on both spawn paths; the kernel
//!   resource limits (setrlimit + cgroup) and the sandbox-exec integration are
//!   deferred to the sandbox layer. A spawn with a fence but no sandbox wired
//!   logs a warning naming what is and is not applied, and runs so the caller
//!   is not blocked.
//!
//! Routing: when stdout or stderr is Piped, the capture path runs (spawn,
//! drain both pipes, wait, return a pre-resolved child). When neither is
//! piped, the async path spawns a live tokio child with kill-on-drop so the
//! caller can await or cancel it.

use std::process::Stdio;
use std::time::Duration;

use crate::launcher::{
    FenceConfig, LauncherChild, LauncherExit, ProcessLauncher, SpawnError, SpawnPolicy,
    SpawnRequest, StdioMode, StdioPipes,
};

/// The default ProcessLauncher: routes spawns through std Command (sync
/// output capture) and tokio Command (async child) behind the trait. Used
/// directly when no sandbox-fenced launcher is wired.
pub struct StdProcessLauncher;

impl StdProcessLauncher {
    /// Construct the default launcher.
    pub fn new() -> Self {
        Self
    }
}

impl Default for StdProcessLauncher {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessLauncher for StdProcessLauncher {
    fn spawn(&self, req: SpawnRequest, policy: SpawnPolicy) -> Result<LauncherChild, SpawnError> {
        if policy.audit {
            audit_log(&req);
        }
        if policy.fence.is_some() {
            // Naming what is applied matters: the wall timeout IS enforced
            // here, so a caller that set a fence for liveness gets it. Only
            // the kernel resource limits are missing, and calling that
            // "unsandboxed" invited the reading that the timeout was dropped
            // too.
            tracing::warn!(
                "[spawn] fence: wall timeout enforced; kernel resource limits \
                 not wired in this launcher"
            );
        }
        if req.interactive {
            spawn_interactive(req)
        } else if req.stdio.stdout == StdioMode::Piped || req.stdio.stderr == StdioMode::Piped {
            spawn_capture(req, policy.fence)
        } else {
            spawn_async(req, policy.fence)
        }
    }
}

fn audit_log(req: &SpawnRequest) {
    let ws = req
        .workspace
        .as_deref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<none>".to_string());
    tracing::warn!(
        "[spawn] audit: program={} args=[{}] workspace={}",
        req.program,
        req.args.join(" "),
        ws
    );
}

fn stdio_for(mode: StdioMode) -> Stdio {
    match mode {
        StdioMode::Inherit => Stdio::inherit(),
        StdioMode::Piped => Stdio::piped(),
        StdioMode::Null => Stdio::null(),
    }
}

/// Capture path: spawn, drain both pipes, wait, and hand back a pre-resolved
/// child so the caller gets the captured output without needing an async
/// runtime. Used when the caller wants piped output (a verify gate parsing
/// command output, a helper printing a secret to stdout).
///
/// The fence wall timeout is enforced here, not only on the async path. A
/// caller that must read a value off stdout has to use this path, so leaving
/// it unbounded meant a child that never exits stalled that caller forever
/// with the fence silently ignored. Past the deadline the child is killed and
/// the wait reports the timeout.
///
/// Both pipes drain on their own threads rather than being read after the
/// wait: a child writing more than the pipe buffer blocks until someone
/// reads, so waiting first while holding an undrained pipe deadlocks on any
/// output larger than the buffer.
#[expect(clippy::disallowed_methods, reason = "infra spawn, not model-driven")]
fn spawn_capture(
    req: SpawnRequest,
    fence: Option<FenceConfig>,
) -> Result<LauncherChild, SpawnError> {
    let mut cmd = std::process::Command::new(&req.program);
    cmd.args(&req.args);
    if let Some(ws) = &req.workspace {
        cmd.current_dir(ws);
    }
    cmd.stdin(stdio_for(req.stdio.stdin));
    cmd.stdout(stdio_for(req.stdio.stdout));
    cmd.stderr(stdio_for(req.stdio.stderr));
    let mut child = cmd.spawn().map_err(|e| SpawnError::Io(e.to_string()))?;
    // A handle is present exactly when its stream was piped, so taking them
    // here preserves the contract that stdout/stderr come back Some only for
    // a piped stream.
    let out_drain = child.stdout.take().map(drain_on_thread);
    let err_drain = child.stderr.take().map(drain_on_thread);
    let status = match fence.map(|f| Duration::from_millis(f.wall_timeout_ms)) {
        Some(limit) => match wait_until(&mut child, limit) {
            Some(status) => status,
            None => {
                // Kill and reap, then let the drain threads finish on the
                // closed pipes so neither is left detached.
                drop(child.kill());
                drop(child.wait());
                drop(out_drain.map(|h| h.join()));
                drop(err_drain.map(|h| h.join()));
                return Ok(LauncherChild::new(
                    None,
                    Box::pin(async move { Err(SpawnError::Io("wall timeout exceeded".into())) }),
                ));
            }
        },
        None => child.wait().map_err(|e| SpawnError::Io(e.to_string()))?,
    };
    let exit_code = status.code();
    let stdout = out_drain.map(|h| joined_text(h.join()));
    let stderr = err_drain.map(|h| joined_text(h.join()));
    Ok(LauncherChild::new(
        None,
        Box::pin(async move {
            Ok(LauncherExit {
                exit_code,
                stdout,
                stderr,
            })
        }),
    ))
}

/// Read a child pipe to end on its own thread so the wait never holds an
/// undrained pipe.
fn drain_on_thread<R: std::io::Read + Send + 'static>(
    mut reader: R,
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        // A read failure yields whatever arrived before it; the exit status
        // carries the real verdict, and losing the tail of a stream must not
        // turn into a spawn failure.
        drop(std::io::Read::read_to_end(&mut reader, &mut buf));
        buf
    })
}

/// Lossy-decode a drained pipe. A panicked drain thread yields empty output
/// rather than propagating: the stream content is best-effort next to the
/// exit status.
fn joined_text(joined: std::thread::Result<Vec<u8>>) -> String {
    String::from_utf8_lossy(&joined.unwrap_or_default()).into_owned()
}

/// Wait for the child up to the limit. Some(status) when it exited in time,
/// None on the deadline or on a wait error (the caller kills and reports the
/// timeout either way). Polls rather than blocking because a bounded wait on
/// a std child has no blocking form; the poll interval only costs latency
/// when a fence is set, and the no-fence path stays a plain blocking wait.
fn wait_until(
    child: &mut std::process::Child,
    limit: Duration,
) -> Option<std::process::ExitStatus> {
    let deadline = std::time::Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {}
            Err(_) => return None,
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Async spawn path: spawn a live tokio child with kill-on-drop. The child
/// handle holds a real async wait future so the caller can await, select, or
/// cancel it. A wall timeout from the fence is applied if set. Used when the
/// caller wants inherited or null stdio (an interactive or fire-and-forget
/// spawn).
#[expect(clippy::disallowed_methods, reason = "infra spawn, not model-driven")]
fn spawn_async(req: SpawnRequest, fence: Option<FenceConfig>) -> Result<LauncherChild, SpawnError> {
    let mut cmd = tokio::process::Command::new(&req.program);
    cmd.args(&req.args);
    if let Some(ws) = &req.workspace {
        cmd.current_dir(ws);
    }
    cmd.kill_on_drop(true);
    cmd.stdin(stdio_for(req.stdio.stdin));
    cmd.stdout(stdio_for(req.stdio.stdout));
    cmd.stderr(stdio_for(req.stdio.stderr));
    let mut child = cmd.spawn().map_err(|e| SpawnError::Io(e.to_string()))?;
    let pid = child.id();
    let wall = fence.map(|f| std::time::Duration::from_millis(f.wall_timeout_ms));
    Ok(LauncherChild::new(
        pid,
        Box::pin(async move {
            let status = match wall {
                Some(timeout) => match tokio::time::timeout(timeout, child.wait()).await {
                    Ok(Ok(s)) => s,
                    Ok(Err(e)) => return Err(SpawnError::Io(e.to_string())),
                    Err(_) => return Err(SpawnError::Io("wall timeout exceeded".into())),
                },
                None => child
                    .wait()
                    .await
                    .map_err(|e| SpawnError::Io(e.to_string()))?,
            };
            Ok(LauncherExit {
                exit_code: status.code(),
                stdout: None,
                stderr: None,
            })
        }),
    ))
}

/// Interactive spawn path: spawn a live std child with piped stdio and hand
/// the live pipe handles back to the caller. The child is wrapped so dropping
/// the handle kills the process (a plain std child is orphaned on drop). Used
/// for a long-lived subprocess the caller speaks a line protocol with — the
/// caller drives the pipes directly and drops the handle to terminate. The
/// wait future is a no-op because the on-drop kill handles termination; a
/// caller that wants the exit code must read it off the child before drop,
/// which this first cut does not surface.
#[expect(clippy::disallowed_methods, reason = "infra spawn, not model-driven")]
fn spawn_interactive(req: SpawnRequest) -> Result<LauncherChild, SpawnError> {
    let mut cmd = std::process::Command::new(&req.program);
    cmd.args(&req.args);
    if let Some(ws) = &req.workspace {
        cmd.current_dir(ws);
    }
    cmd.stdin(stdio_for(req.stdio.stdin));
    cmd.stdout(stdio_for(req.stdio.stdout));
    cmd.stderr(stdio_for(req.stdio.stderr));
    let mut child = cmd.spawn().map_err(|e| SpawnError::Io(e.to_string()))?;
    let pid = child.id();
    // Take the pipes out of the child before moving the child into the
    // on-drop killer; the pipes are handed to the caller, the killer guards
    // the process lifetime.
    let stdin = if req.stdio.stdin == StdioMode::Piped {
        child
            .stdin
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Write + Send>)
    } else {
        None
    };
    let stdout = if req.stdio.stdout == StdioMode::Piped {
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>)
    } else {
        None
    };
    let stderr = if req.stdio.stderr == StdioMode::Piped {
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>)
    } else {
        None
    };
    let killer = KillOnDrop(Some(child));
    Ok(LauncherChild::with_pipes(
        Some(pid),
        StdioPipes {
            stdin,
            stdout,
            stderr,
        },
        // The wait future is a no-op: the on-drop kill on the captured killer
        // terminates the process. A future cut can surface the real exit code
        // by blocking on the child here; the first cut does not need it.
        Box::pin(async move {
            drop(killer);
            Ok(LauncherExit::default())
        }),
    ))
}

/// Wraps a std child so dropping it kills and reaps the process. A plain
/// std::process::Child is orphaned on drop (the process keeps running), which
/// is wrong for an interactive long-lived subprocess the caller loses interest
/// in. The kill is best-effort: a child that already exited is waited
/// silently.
struct KillOnDrop(Option<std::process::Child>);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            // Best-effort kill plus reap so the process does not linger. The
            // results are intentionally ignored: a child that already exited
            // errors here, and there is nothing to recover at drop time.
            drop(child.kill());
            drop(child.wait());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launcher::{SpawnPolicy, SpawnRequest};

    #[tokio::test]
    async fn test_sync_output_captures_stdout() {
        let launcher = StdProcessLauncher::new();
        let req = SpawnRequest::new("echo")
            .with_args(["hello"])
            .piped_output();
        let child = launcher.spawn(req, SpawnPolicy::default()).unwrap();
        let exit = child.wait().await.unwrap();
        assert_eq!(exit.exit_code, Some(0));
        assert_eq!(exit.stdout.as_deref(), Some("hello\n"));
    }

    #[tokio::test]
    async fn test_sync_output_nonzero_exit() {
        let launcher = StdProcessLauncher::new();
        let req = SpawnRequest::new("sh")
            .with_args(["-c", "exit 3"])
            .piped_output();
        let child = launcher.spawn(req, SpawnPolicy::default()).unwrap();
        let exit = child.wait().await.unwrap();
        assert_eq!(exit.exit_code, Some(3));
    }

    #[tokio::test]
    async fn test_sync_output_stderr() {
        let launcher = StdProcessLauncher::new();
        let req = SpawnRequest::new("sh")
            .with_args(["-c", "echo err 1>&2"])
            .piped_output();
        let child = launcher.spawn(req, SpawnPolicy::default()).unwrap();
        let exit = child.wait().await.unwrap();
        assert!(exit.stderr.as_deref().is_some_and(|s| s.contains("err")));
    }

    #[tokio::test]
    async fn test_workspace_as_cwd() {
        let launcher = StdProcessLauncher::new();
        let tmp = std::env::temp_dir();
        // pwd (Git Bash on Windows) translates the Windows path to a Unix
        // form (/tmp for the temp dir), so the basename check fails. Use
        // cmd /c cd on Windows to get the native path; pwd on Unix.
        #[cfg(windows)]
        let (program, args): (&str, Vec<&str>) = ("cmd", vec!["/c", "cd"]);
        #[cfg(not(windows))]
        let (program, args): (&str, Vec<&str>) = ("pwd", vec![]);
        let req = SpawnRequest::new(program)
            .with_args(args)
            .with_workspace(tmp.clone())
            .piped_output();
        let child = launcher.spawn(req, SpawnPolicy::default()).unwrap();
        let exit = child.wait().await.unwrap();
        let out = exit.stdout.unwrap_or_default();
        let basename = tmp
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        assert!(
            out.contains(&basename),
            "cwd output should contain the temp dir basename, got: {out}"
        );
    }

    /// An interactive spawn hands back live stdin/stdout pipe handles the
    /// caller drives directly. Echo a line through a cat child to prove the
    /// pipes are wired both ways.
    #[tokio::test]
    async fn test_interactive_hands_back_pipes() {
        use std::io::{BufRead, BufReader, Write};
        let launcher = StdProcessLauncher::new();
        let req = SpawnRequest::new("cat").interactive();
        let mut child = launcher.spawn(req, SpawnPolicy::default()).unwrap();
        let pipes = child.take_pipes().expect("interactive spawn pipes back");
        let mut stdin = pipes.stdin.expect("stdin piped");
        let stdout = pipes.stdout.expect("stdout piped");
        let mut reader = BufReader::new(stdout);
        stdin.write_all(b"ping\n").unwrap();
        stdin.flush().unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line, "ping\n");
        drop(stdin);
        drop(reader);
        // Dropping the child triggers the on-drop kill so the test process
        // does not leak a cat.
        drop(child.wait().await);
    }

    /// The capture path honors the fence wall timeout. Without it a child that
    /// never exits blocks the caller forever: the capture path is the one a
    /// caller reading a secret off stdout must use, so an unbounded wait there
    /// means a hung helper stalls whatever asked for the secret.
    #[tokio::test]
    async fn test_capture_honors_wall_timeout() {
        let launcher = StdProcessLauncher::new();
        let req = SpawnRequest::new("sleep").with_args(["30"]).piped_output();
        let policy = SpawnPolicy::default().with_fence(FenceConfig {
            wall_timeout_ms: 300,
            ..FenceConfig::default()
        });
        let start = std::time::Instant::now();
        let child = launcher.spawn(req, policy).expect("spawn succeeds");
        let outcome = child.wait().await;
        let elapsed = start.elapsed();
        assert!(
            outcome.is_err(),
            "a child outliving the wall timeout must not report success"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "the wait must be bounded by the timeout, not the child's lifetime; \
             took {elapsed:?}"
        );
    }

    /// A capture spawn with no fence still waits for the child, so the
    /// bounded-wait path does not truncate a slow-but-finishing command.
    #[tokio::test]
    async fn test_capture_without_fence_waits() {
        let launcher = StdProcessLauncher::new();
        let req = SpawnRequest::new("sh")
            .with_args(["-c", "sleep 0.3; echo late"])
            .piped_output();
        let child = launcher.spawn(req, SpawnPolicy::default()).unwrap();
        let exit = child.wait().await.unwrap();
        assert_eq!(exit.exit_code, Some(0));
        assert_eq!(exit.stdout.as_deref(), Some("late\n"));
    }

    /// Output that exceeds the pipe buffer still comes back whole: the drain
    /// runs concurrently with the wait, so a child writing more than the pipe
    /// capacity cannot deadlock against a waiter holding an undrained pipe.
    #[tokio::test]
    async fn test_capture_survives_large_output() {
        let launcher = StdProcessLauncher::new();
        // 400 KiB, well past the typical 64 KiB pipe buffer.
        let req = SpawnRequest::new("sh")
            .with_args(["-c", "yes abcdefghij | head -c 409600"])
            .piped_output();
        let policy = SpawnPolicy::default().with_fence(FenceConfig {
            wall_timeout_ms: 20_000,
            ..FenceConfig::default()
        });
        let child = launcher.spawn(req, policy).unwrap();
        let exit = child.wait().await.unwrap();
        assert_eq!(exit.stdout.map(|s| s.len()), Some(409_600));
    }

    /// A non-interactive spawn leaves pipes None so the take_pipes contract is
    /// unambiguous.
    #[tokio::test]
    async fn test_capture_spawn_no_pipes() {
        let launcher = StdProcessLauncher::new();
        let req = SpawnRequest::new("true").piped_output();
        let mut child = launcher.spawn(req, SpawnPolicy::default()).unwrap();
        assert!(child.take_pipes().is_none());
        drop(child.wait().await);
    }
}
