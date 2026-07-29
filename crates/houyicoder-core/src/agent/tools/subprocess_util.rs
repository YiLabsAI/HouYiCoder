//! Shared subprocess discovery + spawn/cancel plumbing for the grep and
//! glob tools. Locating rg and racing the cancellation token against child
//! exit are identical across both tools, so they live here to avoid drift.
//! An Esc cancel kills the child (SIGTERM via tokio) so CPU stops at once,
//! instead of the spawn_blocking orphan running to completion.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use houyicoder_protocol::extension::ToolError;

/// Locate the rg binary. Ok(Some) means a usable binary was found; None
/// means not found, so the caller falls back to the in-process traversal.
pub(crate) fn find_rg() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("HOUYI_RG_PATH") {
        let candidate = PathBuf::from(p);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join("rg");
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(p) {
        Ok(m) => m.is_file() && m.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    std::fs::metadata(p).map(|m| m.is_file()).unwrap_or(false)
}

/// Spawn rg with the given args and target, race the cancellation token
/// against child exit, drain stdout and stderr into buffers, and return the
/// collected output. Ok(Some) means rg ran to completion; Ok(None) means
/// spawn failed with NotFound (rg vanished between find_rg and spawn) so
/// the caller falls back; Err surfaces a real spawn or exit failure. On
/// cancel the child is killed and reaped so no zombie lingers, then the
/// function returns an interrupted error.
//
// The spawn bypasses the launcher port on purpose. The port today exposes
// sync capture-on-exit (no cancel race), async no-capture (no stdout), and
// interactive live-pipes (no exit code) — none fits a search that needs
// piped output, a cancel-raced wait, an exit code, and a kill. Routing rg
// through the port would require extending it with an async-piped-killable
// spawn; until that lands, rg search is allow-flagged as a trusted
// read-only internal spawn (not a user shellout) so the fence and audit
// policy do not apply to it the way they apply to a shell the model drives.
#[expect(clippy::disallowed_methods, reason = "infra spawn, not model-driven")]
pub(crate) async fn spawn_rg_select_cancel(
    rg: &Path,
    args: &[String],
    target: &Path,
    cancel: Option<&CancellationToken>,
) -> Result<Option<std::process::Output>, ToolError> {
    let mut cmd = Command::new(rg);
    cmd.args(args);
    cmd.arg(target);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    // Guarantee no orphan even if the caller's select! drops this future
    // before the explicit cancel-branch kill runs: an Esc fires the loop's
    // cancelled branch at once, and the tool future can be dropped before
    // its own child.kill await is ever polled. Without kill_on_drop the
    // Child handle would detach on drop, leaving rg running (the exact CPU
    // leak this fix is meant to close). kill_on_drop makes the kill a drop
    // invariant, independent of which select! wins.
    cmd.kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(ToolError::Failed(format!("spawn rg: {e}"))),
    };
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let stdout_task = tokio::task::spawn(async move {
        let mut buf = Vec::new();
        let mut reader = tokio::io::BufReader::new(stdout);
        let _result = reader.read_to_end(&mut buf).await;
        buf
    });
    let stderr_task = tokio::task::spawn(async move {
        let mut buf = Vec::new();
        let mut reader = tokio::io::BufReader::new(stderr);
        let _result = reader.read_to_end(&mut buf).await;
        buf
    });
    let status = match cancel {
        Some(token) => match tokio::select! {
            _ = token.cancelled() => {
                let _kill = child.kill().await;
                let _wait = child.wait().await;
                let _stdout = stdout_task.await;
                let _stderr = stderr_task.await;
                return Err(ToolError::Failed("interrupted by user".into()));
            }
            s = child.wait() => s,
        } {
            Ok(s) => s,
            Err(e) => {
                let _stdout = stdout_task.await;
                let _stderr = stderr_task.await;
                return Err(ToolError::Failed(format!("rg wait: {e}")));
            }
        },
        None => child
            .wait()
            .await
            .map_err(|e| ToolError::Failed(format!("rg wait: {e}")))?,
    };
    let stdout_bytes = stdout_task.await.unwrap_or_default();
    let stderr_bytes = stderr_task.await.unwrap_or_default();
    // rg exits 0 when matches are found, 1 when none are found; both are
    // success. Any other code is an error to surface, not a silent empty.
    if !status.success() && status.code() != Some(1) {
        let stderr_text = String::from_utf8_lossy(&stderr_bytes);
        return Err(ToolError::Failed(format!(
            "rg exited {:?}: {}",
            status.code(),
            stderr_text.trim()
        )));
    }
    Ok(Some(std::process::Output {
        status,
        stdout: stdout_bytes,
        stderr: stderr_bytes,
    }))
}
