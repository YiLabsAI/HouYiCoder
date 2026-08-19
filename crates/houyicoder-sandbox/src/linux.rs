//! Linux sandbox session. Two mutually exclusive cfg paths:
//!
//! - linux plus the enforce feature: a real Landlock fence applied at
//!   construction time via the landlock crate (pure-Rust, safe wrappers
//!   over the Landlock LSM syscalls). The fence is an irreversible
//!   per-process kernel restriction: it tightens the calling process and
//!   every process it later spawns, so exec_with_config runs the command
//!   unfenced at the spawn level and the child inherits the fence.
//! - otherwise (non-Linux, or the enforce feature off): an audited no-op.
//!   Each operation emits a one-line audit to stderr so a future monitor or
//!   log scanner can see the fence was not enforced.
//!
//! Why apply at construction rather than per-exec: the macOS backend fences
//! the child by spawning sandbox-exec (a separate binary that applies the
//! Seatbelt profile in a forked child, leaving the parent unrestricted). The
//! workspace lint denies unsafe_code, so the fork plus pre_exec plus apply
//! pattern is unavailable here — pre_exec is unsafe. Landlock has no
//! equivalent of sandbox-exec as a std-routable helper binary, and the
//! crate's safe API only applies to the calling process. Construction-time
//! application is therefore the safe-code tradeoff: the process that
//! constructs the session becomes the sandbox boundary, and all child
//! commands inherit the fence.
//!
//! Graceful degradation: the landlock crate reports a NotEnforced ruleset
//! status when the kernel has no Landlock support (NotImplemented or
//! NotEnabled LandlockStatus) or the requested features exceed the running
//! kernel ABI. In that case the session stays a no-op fence — exec runs
//! unfenced and the application-level path resolver remains the only
//! boundary. An audit line is emitted so the gap is visible, never silent.

use houyicoder_api::sandbox::{
    Containment, Coverage, FenceStatus, NetworkPolicy, SandboxSession, SideEffect,
};
use houyicoder_async::PFut;
use houyicoder_context::{ExecConfig, ExecResult, SandboxError};
use houyicoder_resilience::resource_breaker::ResourceBreaker;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A Linux sandbox session. The workspace is the user's project dir (Guarded
/// mode) or a temp dir. Drop leaves the workspace untouched when not owned.
/// Under linux plus the enforce feature, construction applies the Landlock
/// fence to the calling process; otherwise the session is an audited no-op.
pub struct LinuxLandlockSession {
    workspace: PathBuf,
    owned: bool,
    fence: FenceStatus,
}

impl LinuxLandlockSession {
    /// Create a session rooted at the user's project dir. The dir is
    /// canonicalized through dunce so symlinks do not trip the path
    /// resolver. The user's directory is never removed on Drop. Construction
    /// applies the Landlock fence (real under linux plus enforce, no-op
    /// otherwise) so the workspace path is in the allow-set.
    pub fn new_in_cwd(cwd: &Path) -> Result<Self, SandboxError> {
        let workspace = dunce::canonicalize(cwd)
            .map_err(|e| SandboxError::Io(format!("cwd canonicalize: {e}")))?;
        let fence = apply_fence(&workspace);
        Ok(Self {
            workspace,
            owned: false,
            fence,
        })
    }

    /// Create a session rooted at a fresh temp dir this session owns. Drop
    /// removes it. Used by tests and as a fallback when no cwd is available.
    pub fn new() -> Result<Self, SandboxError> {
        let workspace = std::env::temp_dir().join(format!(
            "houyicoder-sandbox-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&workspace)?;
        let fence = apply_fence(&workspace);
        Ok(Self {
            workspace,
            owned: true,
            fence,
        })
    }

    /// Attach an aggregate resource breaker. The Landlock fence is applied
    /// at construction and cannot be widened per-exec, so the breaker is
    /// stored but not consulted here — the macOS backend uses it for
    /// per-spawn try_acquire; Linux relies on the wall-timeout plus
    /// kill_on_drop. Accepted for API parity with PlatformSession.
    #[must_use]
    pub fn with_breaker(self, _breaker: Arc<ResourceBreaker>) -> Self {
        self
    }

    /// Set the network posture. The Landlock fence is applied at
    /// construction and is irreversible, so the posture cannot widen or
    /// narrow the fence after the fact. Accepted for API parity with
    /// PlatformSession; the posture is not stored because it has no effect
    /// on this backend.
    #[must_use]
    pub fn with_network(self, _network: NetworkPolicy) -> Self {
        self
    }
}

impl Default for LinuxLandlockSession {
    fn default() -> Self {
        Self::new().expect("linux sandbox session")
    }
}

impl Drop for LinuxLandlockSession {
    fn drop(&mut self) {
        if self.owned {
            let _result = std::fs::remove_dir_all(&self.workspace);
        }
    }
}

// Apply the Landlock fence to the calling process. Under linux plus the
// enforce feature this builds a real ruleset and calls restrict_self;
// otherwise it is a no-op (the stub session logs the unfenced gap from each
// method).
#[cfg(all(target_os = "linux", feature = "enforce"))]
fn apply_fence(workspace: &Path) -> FenceStatus {
    use landlock::{
        ABI, Access, AccessFs, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus,
        path_beneath_rules,
    };

    // Target a recent ABI. The landlock crate degrades gracefully on older
    // kernels via the default BestEffort compatibility level, and
    // restrict_self reports NotEnforced when the kernel has no Landlock at
    // all — that path falls through to the NotEnforced status, never an Err.
    let abi = ABI::V6;

    // System read-only tree: the shell, the dynamic linker, shared libs,
    // /etc config, /proc for ps and /proc/self, /sys for read-only sysfs.
    // from_read includes Execute so binaries under these trees can run.
    const READ_PATHS: &[&str] = &[
        "/usr", "/lib", "/lib64", "/bin", "/sbin", "/etc", "/proc", "/sys",
    ];
    // Read-write: the workspace (the agent edits here). The character
    // devices the runtime needs to open are added separately; path_beneath
    // narrows a file to file-level access automatically.
    let workspace_str = workspace.to_string_lossy();
    let rw_paths: [&str; 1] = [workspace_str.as_ref()];
    const DEV_PATHS: &[&str] = &["/dev/null", "/dev/zero", "/dev/random", "/dev/urandom"];

    let result = (|| {
        let ruleset = Ruleset::default().handle_access(AccessFs::from_all(abi))?;
        let created = ruleset.create()?;
        let created = created
            .add_rules(path_beneath_rules(READ_PATHS, AccessFs::from_read(abi)))?
            .add_rules(path_beneath_rules(rw_paths, AccessFs::from_all(abi)))?
            .add_rules(path_beneath_rules(DEV_PATHS, AccessFs::from_all(abi)))?;
        let status = created.restrict_self()?;
        Ok::<_, landlock::RulesetError>(status)
    })();
    match result {
        Ok(status)
            if matches!(
                status.ruleset,
                RulesetStatus::FullyEnforced | RulesetStatus::PartiallyEnforced
            ) =>
        {
            // Fence applied. Children spawned by exec inherit it.
            FenceStatus::Enforced
        }
        Ok(_) => {
            tracing::warn!(
                "sandbox audit: landlock supported but ruleset not enforced; running unfenced"
            );
            FenceStatus::NotEnforced
        }
        Err(e) => {
            tracing::warn!("sandbox audit: landlock apply failed: {e}; running unfenced");
            FenceStatus::Failed(e.to_string())
        }
    }
}

#[cfg(not(all(target_os = "linux", feature = "enforce")))]
fn apply_fence(_workspace: &Path) -> FenceStatus {
    // No-op: the stub session methods emit their own unfenced audit lines.
    FenceStatus::Unavailable
}

// ----------------------------------------------------------------------------
// SandboxSession impl. Two cfg-gated impls: the real one (linux plus
// enforce) runs commands unfenced at the spawn level (the fence was applied
// at construction and the child inherits it); the stub emits an audit line
// on every call so the unfenced gap is visible.
// ----------------------------------------------------------------------------

#[cfg(all(target_os = "linux", feature = "enforce"))]
impl SandboxSession for LinuxLandlockSession {
    fn fence_status(&self) -> FenceStatus {
        self.fence.clone()
    }

    // The child inherits the Landlock domain applied at construction, so the
    // spawn itself needs no sandbox-exec equivalent. kill_on_drop plus a wall
    // timeout are the resource fence; a full breaker tree-kill guard is the
    // macOS backend's concern and tracked separately for Linux.
    #[expect(clippy::disallowed_methods, reason = "infra spawn, not model-driven")]
    fn exec_with_config(
        &self,
        command: &str,
        config: ExecConfig,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        let cwd = self.workspace.clone();
        let command = command.to_string();
        let wall = std::time::Duration::from_millis(config.wall_timeout_ms);
        Box::pin(async move {
            let _ = (config.cpu_secs, config.as_bytes, config.nproc);
            let mut cmd = tokio::process::Command::new("/bin/sh");
            cmd.arg("-c")
                .arg(&command)
                .current_dir(&cwd)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true);
            cmd.process_group(0);
            let child = cmd
                .spawn()
                .map_err(|e| SandboxError::SandboxUnavailable(format!("spawn: {e}")))?;
            let outcome = tokio::time::timeout(wall, child.wait_with_output()).await;
            match outcome {
                Ok(Ok(output)) => Ok(ExecResult {
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                    exit_code: output.status.code(),
                }),
                Ok(Err(e)) => Err(SandboxError::Io(format!("wait: {e}"))),
                Err(_elapsed) => Err(SandboxError::Timeout(format!(
                    "wall-clock {}s exceeded",
                    config.wall_timeout_ms
                ))),
            }
        })
    }

    fn workspace_root(&self) -> std::sync::Arc<std::path::Path> {
        std::sync::Arc::from(self.workspace.clone())
    }
}

#[cfg(not(all(target_os = "linux", feature = "enforce")))]
impl SandboxSession for LinuxLandlockSession {
    fn fence_status(&self) -> FenceStatus {
        self.fence.clone()
    }

    // No kernel fence available (non-Linux or enforce feature off). The
    // command runs unfenced with an audit line so the gap is visible, not
    // silent. The application-level path resolver is the only boundary.
    #[expect(clippy::disallowed_methods, reason = "infra spawn, not model-driven")]
    fn exec_with_config(
        &self,
        command: &str,
        config: ExecConfig,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        let cwd = self.workspace.clone();
        let command = command.to_string();
        let wall = std::time::Duration::from_millis(config.wall_timeout_ms);
        Box::pin(async move {
            tracing::warn!(
                "sandbox audit: linux landlock NOT enforced; running unfenced (wall={}s)",
                config.wall_timeout_ms
            );
            let _ = (config.cpu_secs, config.as_bytes, config.nproc);
            let mut cmd = tokio::process::Command::new("/bin/sh");
            cmd.arg("-c")
                .arg(&command)
                .current_dir(&cwd)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true);
            cmd.process_group(0);
            let child = cmd
                .spawn()
                .map_err(|e| SandboxError::SandboxUnavailable(format!("spawn: {e}")))?;
            let outcome = tokio::time::timeout(wall, child.wait_with_output()).await;
            match outcome {
                Ok(Ok(output)) => Ok(ExecResult {
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                    exit_code: output.status.code(),
                }),
                Ok(Err(e)) => Err(SandboxError::Io(format!("wait: {e}"))),
                Err(_elapsed) => Err(SandboxError::Timeout(format!(
                    "wall-clock {}s exceeded",
                    config.wall_timeout_ms
                ))),
            }
        })
    }

    fn workspace_root(&self) -> std::sync::Arc<std::path::Path> {
        std::sync::Arc::from(self.workspace.clone())
    }
}

#[cfg(test)]
#[cfg(not(all(target_os = "linux", feature = "enforce")))]
mod tests {
    // On the stub path (non-Linux, or enforce off) the constructor is a
    // pure no-op fence, safe to exercise. On linux plus enforce the
    // constructor applies a real Landlock domain to the test process, so
    // these tests are cfg-gated off there (see the landlock_smoke example
    // for that path).

    use super::*;

    #[test]
    fn test_session_constructs_workspace() {
        let session = LinuxLandlockSession::new().expect("stub session");
        assert!(session.workspace.exists());
        assert!(session.workspace.is_dir());
    }

    #[test]
    fn test_absolute_path_rejected() {
        let session = LinuxLandlockSession::new().expect("stub session");
        let resolved = session.resolve("/etc/passwd");
        assert!(matches!(resolved, Err(SandboxError::PathTraversal(_))));
    }
}

impl Containment for LinuxLandlockSession {
    // Interim: Landlock fences writes, but the writable-roots set is not yet
    // computed here, so coverage reports Unfenced. This is a coverage-gap, not
    // a "Linux has no fence" statement -- the fence exists, its roots are just
    // not surfaced to the gate yet. Until they are, the auto-allow does not
    // fire on Linux and every exec asks.
    fn coverage(&self) -> Coverage {
        Coverage::Unfenced
    }

    fn would_block(&self, _effect: SideEffect) -> Option<String> {
        None
    }
}
