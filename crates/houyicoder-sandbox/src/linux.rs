//! Linux sandbox session: a per-spawn Landlock fence via a helper binary.
//!
//! Each exec spawns the fence helper beside the daemon, passes the live
//! grant set in argv, and the helper applies a Landlock ruleset to itself
//! before exec-ing the command shell: the fence covers the spawned tree
//! only, never the daemon. Construction probes the helper for FenceStatus;
//! unfenced, exec runs directly with an audit line and the path resolver
//! remains the boundary.

use crate::runtime_dirs::RuntimeDirs;
use houyicoder_api::sandbox::{
    Containment, Coverage, FenceStatus, NetworkPolicy, SandboxSession, SideEffect,
    normalize_tool_path,
};
use houyicoder_async::PFut;
use houyicoder_context::{ExecConfig, ExecResult, SandboxError};
use houyicoder_resilience::resource_breaker::ResourceBreaker;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// File name of the fence helper, located beside the daemon executable.
const HELPER_BIN: &str = "houyicoder-sandbox-helper";

/// A Linux sandbox session. The workspace is the user's project dir (Guarded
/// mode) or a temp dir this session owns; Drop removes only an owned one.
/// The fence lives in the spawned helper, so the daemon itself is never
/// restricted and directory grants given mid-session reach the next command.
pub struct LinuxLandlockSession {
    workspace: PathBuf,
    owned: bool,
    fence: FenceStatus,
    /// Scratch dir this session always owns: exported as TMPDIR so heredoc
    /// and mktemp writes land inside the fence, which grants nothing under
    /// the shared system temp root. Removed on Drop.
    tmpdir: PathBuf,
    /// Runtime directory grants, re-read at every spawn so a consent given
    /// mid-session widens the next command's fence.
    dirs: RuntimeDirs,
}

/// Mint the per-session scratch dir under the system temp root. The name
/// mixes pid, a per-process counter and nanos so two sessions can never
/// share one dir; create_dir then fails rather than reusing a stray dir.
fn session_tmpdir() -> Result<PathBuf, SandboxError> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir =
        std::env::temp_dir().join(format!("houyicoder-tmp-{}-{n}-{nanos}", std::process::id()));
    std::fs::create_dir(&dir)?;
    dunce::canonicalize(&dir).map_err(|e| SandboxError::Io(format!("tmpdir canonicalize: {e}")))
}

impl LinuxLandlockSession {
    /// Create a session rooted at the user's project dir. The dir is
    /// canonicalized through dunce so symlinks do not trip the path
    /// resolver. The user's directory is never removed on Drop. Construction
    /// probes the helper once; the status answers fence_status and decides
    /// whether exec routes through the helper or runs with an audit line.
    pub fn new_in_cwd(cwd: &Path) -> Result<Self, SandboxError> {
        let workspace = dunce::canonicalize(cwd)
            .map_err(|e| SandboxError::Io(format!("cwd canonicalize: {e}")))?;
        let fence = probe_fence(&workspace);
        Ok(Self {
            workspace,
            owned: false,
            fence,
            tmpdir: session_tmpdir()?,
            dirs: RuntimeDirs::default(),
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
        let fence = probe_fence(&workspace);
        Ok(Self {
            workspace,
            owned: true,
            fence,
            tmpdir: session_tmpdir()?,
            dirs: RuntimeDirs::default(),
        })
    }

    /// No-op: this backend has no per-spawn hook to consult a breaker from, so
    /// the wall-timeout plus kill_on_drop are the resource fence. Present for
    /// PlatformSession parity.
    #[must_use]
    pub fn with_breaker(self, _breaker: Arc<ResourceBreaker>) -> Self {
        self
    }

    /// No-op: the path fence carries no network ruleset yet, so a posture set
    /// here cannot be honored. Present for PlatformSession parity; the gap is
    /// visible through would_block answering None for network effects.
    #[must_use]
    pub fn with_network(self, _network: NetworkPolicy) -> Self {
        self
    }

    /// Locate the fence helper: an explicit environment override wins, then
    /// the directory of the running executable, then one level up (build and
    /// test layouts place the daemon in a deps directory below the helper).
    fn helper_path() -> Option<PathBuf> {
        if let Some(path) = std::env::var_os("HOUYICODER_SANDBOX_HELPER") {
            return Some(PathBuf::from(path));
        }
        let deps = std::env::current_exe().ok()?.parent()?.to_path_buf();
        let sibling = deps.join(HELPER_BIN);
        if sibling.is_file() {
            return Some(sibling);
        }
        let upper = deps.parent()?.join(HELPER_BIN);
        if upper.is_file() {
            return Some(upper);
        }
        None
    }

    fn resolve_path(&self, path: &str, include_read_only: bool) -> Result<PathBuf, SandboxError> {
        let supplied = Path::new(path);
        let base = if supplied.is_absolute() {
            supplied.to_path_buf()
        } else {
            self.workspace.join(path)
        };
        let canonical = normalize_tool_path(&base).unwrap_or(base);
        if canonical.starts_with(&self.workspace) || canonical.starts_with(&self.tmpdir) {
            return Ok(canonical);
        }
        if self.dirs.allows_write(&canonical) {
            return Ok(canonical);
        }
        if include_read_only && self.dirs.allows_read(&canonical) {
            return Ok(canonical);
        }
        Err(SandboxError::PathTraversal(format!(
            "path escapes workspace + authorized dirs: {path}"
        )))
    }

    /// Run one command. Fenced, the helper is spawned with the live grant
    /// set in argv; unfenced, the shell is spawned directly after an audit
    /// line. Both paths share the cwd, the pipes, the scratch TMPDIR, the
    /// process group, kill_on_drop and the wall timeout. The cpu, address
    /// space and process count budgets are not passed to the helper: their
    /// per-spawn rlimit semantics are per user, not per tree, so the wall
    /// timeout plus the group kill below are the resource fence here.
    #[expect(clippy::disallowed_methods, reason = "infra spawn, not model-driven")]
    async fn exec_inner(
        &self,
        command: String,
        config: ExecConfig,
    ) -> Result<ExecResult, SandboxError> {
        let wall = std::time::Duration::from_millis(config.wall_timeout_ms);
        let helper = if matches!(self.fence, FenceStatus::Enforced) {
            Self::helper_path()
        } else {
            None
        };
        let mut cmd = match helper {
            Some(helper) => {
                let mut cmd = tokio::process::Command::new(helper);
                cmd.arg("--write").arg(&self.workspace);
                cmd.arg("--write").arg(&self.tmpdir);
                for dir in self.dirs.read_write() {
                    cmd.arg("--write").arg(dir);
                }
                for dir in self.dirs.read_only() {
                    cmd.arg("--read").arg(dir);
                }
                cmd.arg("--").arg(&command);
                cmd
            }
            None => {
                tracing::warn!(
                    "sandbox audit: landlock fence NOT enforced; running unfenced (wall={}ms)",
                    config.wall_timeout_ms
                );
                let mut cmd = tokio::process::Command::new("/bin/sh");
                cmd.arg("-c").arg(&command);
                cmd
            }
        };
        // Export the session scratch dir as TMPDIR so heredoc temp files and
        // tools that honor TMPDIR land inside the fence; the shared system
        // temp root is never granted. TMPPREFIX routes zsh heredoc temps
        // there too. Set explicitly rather than inherited, since the parent
        // TMPDIR points outside the fence.
        let tmpdir_str = self.tmpdir.to_string_lossy().into_owned();
        cmd.env("TMPDIR", &tmpdir_str)
            .env("TMPPREFIX", format!("{tmpdir_str}/zsh"))
            .current_dir(&self.workspace)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        cmd.process_group(0);
        let child = cmd
            .spawn()
            .map_err(|e| SandboxError::SandboxUnavailable(format!("spawn: {e}")))?;
        // After process_group(0) the child's group id equals its pid. The
        // guard reaps the whole tree on every exit path: kill_on_drop only
        // reaches the direct child, so without it a wall timeout or a
        // dropped future would leave grandchildren running.
        let pgid = child.id().unwrap_or(0) as i32;
        let _tree_guard = TreeKillGuard { pgid };
        let outcome = tokio::time::timeout(wall, child.wait_with_output()).await;
        match outcome {
            Ok(Ok(output)) => Ok(ExecResult {
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                exit_code: output.status.code(),
            }),
            Ok(Err(e)) => Err(SandboxError::Io(format!("wait: {e}"))),
            Err(_elapsed) => Err(SandboxError::Timeout(format!(
                "wall-clock {}ms exceeded",
                config.wall_timeout_ms
            ))),
        }
    }
}

/// Group kill on drop: SIGKILL every process in the child's group, so a
/// timeout or a cancelled future does not leave fenced grandchildren burning
/// CPU and holding locks after the daemon has already returned.
struct TreeKillGuard {
    pgid: i32,
}

impl Drop for TreeKillGuard {
    fn drop(&mut self) {
        if self.pgid > 0 {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(-self.pgid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
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
        // The scratch dir is always created by this session, so it is always
        // removed here. Best-effort; never panic.
        let _result = std::fs::remove_dir_all(&self.tmpdir);
    }
}

/// Ask the helper whether it can enforce on this kernel. The answer maps one
/// to one onto FenceStatus; a missing helper, a refused spawn or a non-zero
/// exit all land in Failed so the composition root surfaces the gap instead
/// of silently degrading.
#[cfg(feature = "enforce")]
fn probe_fence(workspace: &Path) -> FenceStatus {
    let Some(helper) = LinuxLandlockSession::helper_path() else {
        tracing::warn!("sandbox audit: fence helper not found beside the daemon; running unfenced");
        return FenceStatus::Failed("fence helper not found".into());
    };
    #[expect(clippy::disallowed_methods, reason = "infra spawn, not model-driven")]
    let probe = std::process::Command::new(&helper)
        .arg("--probe")
        .arg("--write")
        .arg(workspace)
        .output();
    match probe {
        Ok(output) if output.status.success() => {
            let word = String::from_utf8_lossy(&output.stdout);
            match word.trim() {
                "enforced" => FenceStatus::Enforced,
                "enforced-partial" => {
                    tracing::warn!(
                        "sandbox audit: landlock enforced with a degraded kernel ABI; some filesystem rights are not restricted"
                    );
                    FenceStatus::Enforced
                }
                "not-enforced" => {
                    tracing::warn!(
                        "sandbox audit: landlock supported but ruleset not enforced; running unfenced"
                    );
                    FenceStatus::NotEnforced
                }
                "unavailable" => {
                    tracing::warn!(
                        "sandbox audit: landlock unavailable on this kernel; running unfenced"
                    );
                    FenceStatus::Unavailable
                }
                other => {
                    let reason = other.strip_prefix("failed:").unwrap_or(other).to_string();
                    tracing::warn!(
                        "sandbox audit: landlock apply failed: {reason}; running unfenced"
                    );
                    FenceStatus::Failed(reason)
                }
            }
        }
        Ok(output) => {
            let reason = format!("probe exited with {}", output.status);
            tracing::warn!("sandbox audit: fence helper probe failed: {reason}; running unfenced");
            FenceStatus::Failed(reason)
        }
        Err(e) => {
            tracing::warn!("sandbox audit: fence helper probe spawn failed: {e}; running unfenced");
            FenceStatus::Failed(format!("probe spawn: {e}"))
        }
    }
}

#[cfg(not(feature = "enforce"))]
fn probe_fence(_workspace: &Path) -> FenceStatus {
    FenceStatus::Unavailable
}

impl SandboxSession for LinuxLandlockSession {
    fn fence_status(&self) -> FenceStatus {
        self.fence.clone()
    }

    fn as_containment(&self) -> Option<&dyn Containment> {
        Some(self)
    }

    fn exec_with_config(
        &self,
        command: &str,
        config: ExecConfig,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        let command = command.to_string();
        Box::pin(async move { self.exec_inner(command, config).await })
    }

    fn workspace_root(&self) -> Arc<Path> {
        Arc::from(self.workspace.clone())
    }

    fn resolve(&self, path: &str) -> Result<PathBuf, SandboxError> {
        self.resolve_path(path, true)
    }

    fn resolve_write(&self, path: &str) -> Result<PathBuf, SandboxError> {
        self.resolve_path(path, false)
    }

    fn add_working_dir(&self, path: &str) -> Result<(), SandboxError> {
        self.dirs.add_write(path)
    }

    fn add_reading_dir(&self, path: &str) -> Result<(), SandboxError> {
        self.dirs.add_read(path)
    }

    fn remove_working_dir(&self, path: &str) {
        self.dirs.remove(path);
    }

    fn working_dirs(&self) -> Vec<String> {
        self.dirs.all_strings()
    }

    fn reading_dirs(&self) -> Vec<String> {
        self.dirs
            .read_only()
            .into_iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect()
    }

    fn writing_dirs(&self) -> Vec<String> {
        self.dirs
            .read_write()
            .into_iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect()
    }
}

/// Map fence state and grant lists onto the coverage answer. Split out of
/// the trait impl so the mapping is testable without a live fence: the roots
/// a Fenced answer certifies must equal the grant set the next helper spawn
/// receives in argv. The session scratch dir is granted too but is not a
/// user-authorized root, so it is not listed.
fn coverage_for(fence: &FenceStatus, workspace: &Path, dirs: &RuntimeDirs) -> Coverage {
    if matches!(fence, FenceStatus::Enforced) {
        let mut roots = vec![workspace.to_path_buf()];
        roots.extend(dirs.read_write());
        Coverage::Fenced {
            writable_roots: roots,
        }
    } else {
        Coverage::Unfenced
    }
}

impl Containment for LinuxLandlockSession {
    /// Fenced, the writable roots are the workspace plus every runtime write
    /// grant; the next helper spawn receives these plus the session scratch
    /// dir, so the certified set is never wider than the kernel grant set.
    /// Unfenced, the gate keeps asking for consent because the resolver is
    /// the only boundary. The answer certifies path containment only: this
    /// backend imposes no network ruleset, so egress from a fenced command is
    /// not contained.
    fn coverage(&self) -> Coverage {
        coverage_for(&self.fence, &self.workspace, &self.dirs)
    }

    fn would_block(&self, _effect: SideEffect) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    // Construction probes the fence helper but never fences this process, so
    // these tests run on any Linux host regardless of kernel support: the
    // probe answer depends on the kernel, the resolver, grant and coverage
    // behavior do not.

    use super::*;

    fn unique_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "houyicoder-linux-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ))
    }

    #[test]
    fn test_session_constructs_workspace() {
        let session = LinuxLandlockSession::new().expect("session");
        assert!(session.workspace.exists());
        assert!(session.workspace.is_dir());
        assert!(session.tmpdir.is_dir());
    }

    #[test]
    fn test_absolute_path_rejected() {
        let session = LinuxLandlockSession::new().expect("session");
        let resolved = session.resolve("/etc/passwd");
        assert!(matches!(resolved, Err(SandboxError::PathTraversal(_))));
    }

    #[test]
    fn test_granted_dir_admitted() {
        let session = LinuxLandlockSession::new().expect("session");
        let granted = unique_dir("grant");
        std::fs::create_dir_all(&granted).expect("create granted dir");
        let target = granted.join("notes.txt");
        std::fs::write(&target, "x").expect("write target");
        let granted_str = granted.to_str().expect("granted str");
        let target_str = target.to_str().expect("target str");

        session.add_working_dir(granted_str).expect("grant");
        let resolved = session.resolve_write(target_str).expect("admitted");
        let canonical_granted = dunce::canonicalize(&granted).expect("canonical granted");
        assert!(resolved.starts_with(canonical_granted));

        session.remove_working_dir(granted_str);
        let revoked = session.resolve_write(target_str);
        assert!(matches!(revoked, Err(SandboxError::PathTraversal(_))));

        let _cleanup = std::fs::remove_dir_all(&granted);
    }

    #[test]
    fn test_scratch_dir_admitted() {
        let session = LinuxLandlockSession::new().expect("session");
        let target = session.tmpdir.join("scratch.txt");
        let target_str = target.to_str().expect("target str");
        let resolved = session.resolve_write(target_str).expect("scratch admitted");
        assert!(resolved.starts_with(&session.tmpdir));
    }

    #[test]
    fn test_coverage_lists_grants() {
        let mut session = LinuxLandlockSession::new().expect("session");
        session.fence = FenceStatus::Enforced;
        let granted = unique_dir("coverage");
        std::fs::create_dir_all(&granted).expect("create granted dir");
        let granted_str = granted.to_str().expect("granted str");
        session.add_working_dir(granted_str).expect("grant");

        match session.coverage() {
            Coverage::Fenced { writable_roots } => {
                assert!(
                    writable_roots.iter().any(|r| r == &session.workspace),
                    "the workspace must be a fenced root: {writable_roots:?}"
                );
                let canonical = dunce::canonicalize(&granted).expect("canonical granted");
                assert!(
                    writable_roots.contains(&canonical),
                    "every write grant must be a fenced root: {writable_roots:?}"
                );
                assert!(
                    !writable_roots.contains(&session.tmpdir),
                    "the scratch dir is not a user-authorized root: {writable_roots:?}"
                );
            }
            other => panic!("an enforced fence must report fenced coverage: {other:?}"),
        }

        session.remove_working_dir(granted_str);
        match session.coverage() {
            Coverage::Fenced { writable_roots } => {
                assert_eq!(writable_roots, vec![session.workspace.clone()]);
            }
            other => panic!("revoking a grant must not unfence: {other:?}"),
        }
        let _cleanup = std::fs::remove_dir_all(&granted);
    }

    #[test]
    fn test_coverage_unfenced() {
        let mut session = LinuxLandlockSession::new().expect("session");
        session.fence = FenceStatus::Failed("probe".into());
        assert!(matches!(session.coverage(), Coverage::Unfenced));
        session.fence = FenceStatus::NotEnforced;
        assert!(matches!(session.coverage(), Coverage::Unfenced));
    }
}
