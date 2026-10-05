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
use std::ffi::OsString;
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

    /// The fence helper argv for one command: the workspace and the session
    /// scratch dir as write grants, every runtime write grant, every read-only
    /// grant, then the command after the separator. Split out of exec_inner so
    /// the argv shape is testable without a spawn: this list IS the kernel
    /// grant set, and a path the resolver admits but this list omits would be
    /// fenced away at spawn time.
    fn helper_argv(&self, command: &str) -> Vec<OsString> {
        let mut argv: Vec<OsString> = Vec::new();
        argv.push("--write".into());
        argv.push(self.workspace.as_os_str().to_os_string());
        argv.push("--write".into());
        argv.push(self.tmpdir.as_os_str().to_os_string());
        for dir in self.dirs.read_write() {
            argv.push("--write".into());
            argv.push(dir.into_os_string());
        }
        for dir in self.dirs.read_only() {
            argv.push("--read".into());
            argv.push(dir.into_os_string());
        }
        argv.push("--".into());
        argv.push(command.into());
        argv
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
                cmd.args(self.helper_argv(&command));
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
            Ok(Ok(output)) => Ok(exec_result_from_output(output)),
            Ok(Err(e)) => Err(SandboxError::Io(format!("wait: {e}"))),
            Err(_elapsed) => Err(SandboxError::Timeout(format!(
                "wall-clock {}ms exceeded",
                config.wall_timeout_ms
            ))),
        }
    }
}

/// Map a finished child's raw output onto the exec answer. Split out of
/// exec_inner so the stream and exit-code mapping is testable without a
/// spawn: a signal death reports a None code, and the lossy conversion is
/// what every caller sees.
fn exec_result_from_output(output: std::process::Output) -> ExecResult {
    ExecResult {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        exit_code: output.status.code(),
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

/// Map the helper probe's answer word onto the fence status. Split out of
/// probe_fence so every branch of the helper protocol is testable without a
/// Linux host or a spawned helper: the word set is the contract between the
/// daemon and the helper binary, and an unrecognized word must land in
/// Failed with the reason, never be mistaken for a working fence.
#[cfg(feature = "enforce")]
fn fence_from_probe(word: &str) -> FenceStatus {
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
            tracing::warn!("sandbox audit: landlock unavailable on this kernel; running unfenced");
            FenceStatus::Unavailable
        }
        other => {
            let reason = other.strip_prefix("failed:").unwrap_or(other).to_string();
            tracing::warn!("sandbox audit: landlock apply failed: {reason}; running unfenced");
            FenceStatus::Failed(reason)
        }
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
            fence_from_probe(&String::from_utf8_lossy(&output.stdout))
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
    use std::fs;
    use std::os::unix::process::ExitStatusExt;
    use std::process::{ExitStatus, Output};

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
        fs::create_dir_all(&granted).expect("create granted dir");
        let target = granted.join("notes.txt");
        fs::write(&target, "x").expect("write target");
        let granted_str = granted.to_str().expect("granted str");
        let target_str = target.to_str().expect("target str");

        session.add_working_dir(granted_str).expect("grant");
        let resolved = session.resolve_write(target_str).expect("admitted");
        let canonical_granted = dunce::canonicalize(&granted).expect("canonical granted");
        assert!(resolved.starts_with(canonical_granted));

        session.remove_working_dir(granted_str);
        let revoked = session.resolve_write(target_str);
        assert!(matches!(revoked, Err(SandboxError::PathTraversal(_))));

        let _cleanup = fs::remove_dir_all(&granted);
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
        fs::create_dir_all(&granted).expect("create granted dir");
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
        let _cleanup = fs::remove_dir_all(&granted);
    }

    #[test]
    fn test_coverage_unfenced() {
        let mut session = LinuxLandlockSession::new().expect("session");
        session.fence = FenceStatus::Failed("probe".into());
        assert!(matches!(session.coverage(), Coverage::Unfenced));
        session.fence = FenceStatus::NotEnforced;
        assert!(matches!(session.coverage(), Coverage::Unfenced));
    }

    /// The argv is the kernel grant set: the workspace and scratch dir come
    /// first as write grants, runtime grants follow in flag pairs by kind,
    /// and the command sits alone after the separator.
    #[test]
    fn test_helper_argv_grants() {
        let session = LinuxLandlockSession::new().expect("session");
        let write_dir = unique_dir("argv-write");
        fs::create_dir_all(&write_dir).expect("create write dir");
        let read_dir = unique_dir("argv-read");
        fs::create_dir_all(&read_dir).expect("create read dir");
        session
            .add_working_dir(write_dir.to_str().expect("write str"))
            .expect("grant write");
        session
            .add_reading_dir(read_dir.to_str().expect("read str"))
            .expect("grant read");

        let argv = session.helper_argv("touch x");
        let canonical_write = dunce::canonicalize(&write_dir).expect("canonical write");
        let canonical_read = dunce::canonicalize(&read_dir).expect("canonical read");
        let expected: Vec<OsString> = vec![
            "--write".into(),
            session.workspace.as_os_str().to_os_string(),
            "--write".into(),
            session.tmpdir.as_os_str().to_os_string(),
            "--write".into(),
            canonical_write.into_os_string(),
            "--read".into(),
            canonical_read.into_os_string(),
            "--".into(),
            "touch x".into(),
        ];
        assert_eq!(argv, expected, "argv must carry the full grant set");

        let _cleanup = fs::remove_dir_all(&write_dir);
        let _cleanup = fs::remove_dir_all(&read_dir);
    }

    /// Every word of the helper protocol, including the ones a healthy host
    /// never sees: an unrecognized word must land in Failed carrying the
    /// reason, never be mistaken for a working fence.
    #[cfg(feature = "enforce")]
    #[test]
    fn test_probe_word_mapping() {
        assert!(matches!(
            fence_from_probe("enforced\n"),
            FenceStatus::Enforced
        ));
        assert!(matches!(
            fence_from_probe("enforced-partial"),
            FenceStatus::Enforced
        ));
        assert!(matches!(
            fence_from_probe("not-enforced"),
            FenceStatus::NotEnforced
        ));
        assert!(matches!(
            fence_from_probe("unavailable"),
            FenceStatus::Unavailable
        ));
        match fence_from_probe("failed:abi too old") {
            FenceStatus::Failed(reason) => assert_eq!(reason, "abi too old"),
            other => panic!("a failed probe must carry its reason: {other:?}"),
        }
        match fence_from_probe("garbage") {
            FenceStatus::Failed(reason) => assert_eq!(reason, "garbage"),
            other => panic!("an unknown word must land in Failed: {other:?}"),
        }
    }

    /// The stream and exit-code mapping every caller sees. The raw wait
    /// status carries the exit code in its upper bits; a signal death would
    /// report None instead, which the Option in the answer preserves.
    #[test]
    fn test_output_maps_result() {
        let output = Output {
            status: ExitStatus::from_raw(3 << 8),
            stdout: b"out".to_vec(),
            stderr: b"err".to_vec(),
        };
        let result = exec_result_from_output(output);
        assert_eq!(result.stdout, "out");
        assert_eq!(result.stderr, "err");
        assert_eq!(result.exit_code, Some(3));
    }
}
