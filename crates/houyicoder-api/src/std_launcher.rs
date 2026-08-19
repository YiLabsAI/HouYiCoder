//! The default launcher implementation: routes every spawn through a std
//! Command (sync output capture) or a tokio Command (async child with
//! kill-on-drop) behind the ProcessLauncher trait. This is the legitimate home
//! for raw Command usage outside the sandbox crate, so the clippy disallowed
//! methods rule is allowed on the two spawn paths below.
//!
//! Policy handling is best-effort for the first cut:
//! - audit: emits a structured spawn log line to stderr.
//! - fence: the wall timeout is applied on the async path via tokio timeout;
//!   the kernel fence (setrlimit + cgroup) and the sandbox-exec integration are
//!   deferred to the sandbox layer. A spawn with a fence but no sandbox wired
//!   logs a warning and runs unsandboxed so the caller is not blocked.
//!
//! Routing: when stdout or stderr is Piped, the sync output path runs
//! (std Command output, captures both, returns a pre-resolved child). When
//! neither is piped, the async path spawns a live tokio child with
//! kill-on-drop so the caller can await or cancel it.

use std::process::Stdio;

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
            tracing::warn!(
                "[spawn] fence requested but kernel fence not yet wired; \
                 spawning unsandboxed"
            );
        }
        if req.interactive {
            spawn_interactive(req)
        } else if req.stdio.stdout == StdioMode::Piped || req.stdio.stderr == StdioMode::Piped {
            spawn_sync_output(req)
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

/// Sync output path: spawn, wait, and capture stdout+stderr in one blocking
/// call. The child handle holds a pre-resolved wait future so the caller gets
/// the captured output without needing an async runtime. Used when the caller
/// wants piped output (a verify gate parsing command output).
#[expect(clippy::disallowed_methods, reason = "infra spawn, not model-driven")]
fn spawn_sync_output(req: SpawnRequest) -> Result<LauncherChild, SpawnError> {
    let mut cmd = std::process::Command::new(&req.program);
    cmd.args(&req.args);
    if let Some(ws) = &req.workspace {
        cmd.current_dir(ws);
    }
    cmd.stdin(stdio_for(req.stdio.stdin));
    cmd.stdout(stdio_for(req.stdio.stdout));
    cmd.stderr(stdio_for(req.stdio.stderr));
    let output = cmd.output().map_err(|e| SpawnError::Io(e.to_string()))?;
    let exit_code = output.status.code();
    let stdout = if req.stdio.stdout == StdioMode::Piped {
        Some(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        None
    };
    let stderr = if req.stdio.stderr == StdioMode::Piped {
        Some(String::from_utf8_lossy(&output.stderr).into_owned())
    } else {
        None
    };
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
