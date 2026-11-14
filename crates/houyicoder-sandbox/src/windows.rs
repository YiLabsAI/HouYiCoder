//! Windows sandbox session: one kernel job per command. The command enters
//! the job at creation, so no instruction of it runs outside the fence, and
//! the job ends with the call -- on completion, on the wall timeout, or when
//! the capture refuses output -- so a grandchild cannot outlive the command
//! that started it. With the enforce feature off, or on another platform, the
//! session is a no-op fence whose methods log the gap.

use crate::output_capture::{CaptureChild, CapturedOutput, DRAIN_GRACE, capture_streams};
use crate::runtime_dirs::RuntimeDirs;
use crate::shell_selection::ShellProgram;
use crate::timeout::message;
use houyicoder_api::capture::RawOutputReceiver;
use houyicoder_api::sandbox::{
    Containment, Coverage, FenceStatus, NetworkPolicy, SandboxSession, SideEffect,
    normalize_tool_path,
};
use houyicoder_api::shell::Shell;
use houyicoder_async::PFut;
use houyicoder_context::{ExecConfig, ExecResult, SandboxError};
use houyicoder_platform::host::process::{
    NativeChild, build_cmd_line, build_command_line, spawn_fenced, spawn_unfenced,
};
use houyicoder_resilience::resource_breaker::ResourceBreaker;
use std::env::temp_dir;
use std::fs::{create_dir_all, remove_dir_all};
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::ExitStatus;
use std::process::id;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::timeout;

/// A Windows sandbox session. The workspace is the user's project dir (Guarded
/// mode) or a temp dir. Drop leaves the workspace untouched when not owned.
/// Under windows plus the enforce feature, each exec creates a per-command Job
/// Object that bounds exactly that command's tree; otherwise the session is a
/// no-op fence and each command runs with the wall timeout alone.
pub struct WindowsJobSession {
    workspace: PathBuf,
    owned: bool,
    fence: FenceStatus,
    /// Runtime directory grants. The job object carries no path primitive,
    /// so these widen the resolver only: the kernel fence stays limited to
    /// process lifetime and the coverage answer remains honest about that.
    dirs: RuntimeDirs,
    /// The shell every command in this session is passed to. None until the
    /// composition root has chosen one, which reads as the command
    /// interpreter: the session names the shell it runs, so the tool layer
    /// never translates for a shell the session does not use.
    program: Option<ShellProgram>,
}

impl WindowsJobSession {
    /// Create a session rooted at the user's project dir. The dir is
    /// canonicalized through dunce so the UNC prefix std canonicalize yields
    /// on Windows does not break downstream string ops. The user's directory
    /// is never removed on Drop. The kernel fence is per command, so
    /// construction only settles the fence status.
    pub fn new_in_cwd(cwd: &Path) -> Result<Self, SandboxError> {
        let workspace = dunce::canonicalize(cwd)
            .map_err(|e| SandboxError::Io(format!("cwd canonicalize: {e}")))?;
        let fence = apply_fence();
        Ok(Self {
            workspace,
            owned: false,
            fence,
            dirs: RuntimeDirs::default(),
            program: None,
        })
    }

    /// Create a session rooted at a fresh temp dir this session owns. Drop
    /// removes it. Used by tests and as a fallback when no cwd is available.
    pub fn new() -> Result<Self, SandboxError> {
        let mut workspace = temp_dir();
        workspace.push(format!(
            "houyicoder-sandbox-{}-{}",
            id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        create_dir_all(&workspace)?;
        let fence = apply_fence();
        Ok(Self {
            workspace,
            owned: true,
            fence,
            dirs: RuntimeDirs::default(),
            program: None,
        })
    }

    /// No-op: a per-command job needs no breaker. Present for
    /// PlatformSession parity.
    #[must_use]
    pub fn with_breaker(self, _breaker: Arc<ResourceBreaker>) -> Self {
        self
    }

    /// No-op: a job object does not fence network access. Present for
    /// PlatformSession parity.
    #[must_use]
    pub fn with_network(self, _network: NetworkPolicy) -> Self {
        self
    }

    /// Set the shell this session passes commands to. The chosen program and
    /// the shell the session reports come from this one field, so a command is
    /// translated for the program that will read it.
    #[must_use]
    pub fn with_shell_program(mut self, program: Option<ShellProgram>) -> Self {
        self.program = program;
        self
    }

    fn resolve_path(&self, path: &str, include_read_only: bool) -> Result<PathBuf, SandboxError> {
        let supplied = Path::new(path);
        let base = if supplied.is_absolute() {
            supplied.to_path_buf()
        } else {
            self.workspace.join(path)
        };
        let canonical = normalize_tool_path(&base).unwrap_or(base);
        if canonical.starts_with(&self.workspace) {
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
}

impl Default for WindowsJobSession {
    fn default() -> Self {
        Self::new().expect("windows sandbox session")
    }
}

impl Drop for WindowsJobSession {
    fn drop(&mut self) {
        // Each command owns its job, and the command guard already closed it,
        // so Drop only removes the workspace this session created.
        if self.owned {
            let _result = remove_dir_all(&self.workspace);
        }
    }
}

// ----------------------------------------------------------------------------
// Fence status. The launcher creates the job per command, so construction
// only settles whether this session claims the fence at all; with the
// enforce feature off the stub session logs the unfenced gap from each
// method.
// ----------------------------------------------------------------------------

#[cfg(all(target_os = "windows", feature = "enforce"))]
fn apply_fence() -> FenceStatus {
    // The no-sandbox flag and the test hatch both mean the session claims no
    // kernel fence: no job is created, so the status reports not enforced and
    // every exec runs with the wall timeout and path resolver alone.
    if crate::is_unfenced() {
        tracing::warn!("sandbox audit: job object enforcement skipped; running unfenced");
        return FenceStatus::NotEnforced;
    }
    FenceStatus::Enforced
}

#[cfg(not(all(target_os = "windows", feature = "enforce")))]
fn apply_fence() -> FenceStatus {
    // No-op: the stub session methods emit their own unfenced audit lines.
    FenceStatus::Unavailable
}

impl CaptureChild for NativeChild {
    fn wait(&mut self) -> Pin<Box<dyn Future<Output = io::Result<ExitStatus>> + Send + '_>> {
        Box::pin(NativeChild::wait(self))
    }

    fn start_kill(&mut self) -> io::Result<()> {
        self.end_tree();
        Ok(())
    }
}

/// Build the command line for the shell this session runs, in the form that
/// shell parses. The command interpreter has its own parser; every other
/// program reads its command line through the C runtime rules.
fn command_line(shell: &ShellProgram, command: &str) -> String {
    let program = shell.executable();
    let program = program.to_string_lossy();
    if matches!(shell, ShellProgram::Cmd) {
        return build_cmd_line(&program, command);
    }
    let arguments = shell.arguments(command);
    let references: Vec<&str> = arguments.iter().map(String::as_str).collect();
    build_command_line(&program, &references)
}

impl WindowsJobSession {
    /// Run one command, feeding its raw output to a capture when one is
    /// given. The launcher creates the job and starts the command inside it;
    /// the drain feeds the receiver as bytes arrive, so a capture that
    /// refuses a chunk ends the command before it runs to its wall budget.
    async fn exec_inner(
        &self,
        command: String,
        config: ExecConfig,
        receiver: Option<Arc<dyn RawOutputReceiver>>,
    ) -> Result<ExecResult, SandboxError> {
        let wall = Duration::from_millis(config.wall_timeout_ms);
        // The job bounds the tree's lifetime and nothing else: no CPU, memory,
        // or process-count limit is applied, so those budget fields reach no
        // kernel limit on this target.
        let _ = (config.cpu_secs, config.as_bytes, config.nproc);
        if self.fence.is_unfenced() {
            tracing::warn!(
                "sandbox audit: windows job object NOT enforced; running unfenced (wall={}s)",
                config.wall_timeout_ms
            );
        }
        let shell = self.program.clone().unwrap_or(ShellProgram::Cmd);
        let line = command_line(&shell, &command);
        // A fenced session gets the job; an unfenced or unavailable one runs
        // the command with the wall timeout alone.
        let started = if matches!(self.fence, FenceStatus::Enforced) {
            spawn_fenced(&line, &self.workspace).await
        } else {
            spawn_unfenced(&line, &self.workspace).await
        };
        let mut child =
            started.map_err(|e| SandboxError::SandboxUnavailable(format!("spawn: {e}")))?;
        let stdout = child.take_stdout().ok_or_else(|| {
            SandboxError::SandboxUnavailable("the command's output is not piped".to_string())
        })?;
        let stderr = child.take_stderr().ok_or_else(|| {
            SandboxError::SandboxUnavailable("the command's error stream is not piped".to_string())
        })?;
        // The bytes land in collected as they arrive, so a wall-clock stop
        // still reports what the command printed before the tree ended.
        let collected = CapturedOutput::default();
        let outcome = timeout(
            wall,
            capture_streams(
                &mut child,
                stdout,
                stderr,
                None,
                &collected,
                DRAIN_GRACE,
                receiver.as_deref(),
            ),
        )
        .await;
        // The tree ends with the call in every outcome: the timeout ends the
        // whole job rather than the direct process only, and the child guard
        // drops on the return path even when the capture itself errored.
        if matches!(outcome, Err(ref _elapsed)) {
            child.end_tree();
        }
        match outcome {
            Ok(Ok(output)) => Ok(ExecResult {
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                exit_code: output.status.code(),
            }),
            Ok(Err(e)) => Err(SandboxError::Io(format!("drain: {e}"))),
            Err(_elapsed) => Err(collected.timeout_error(message(config.wall_timeout_ms))),
        }
    }
}

impl SandboxSession for WindowsJobSession {
    fn shell(&self) -> Shell {
        self.program
            .as_ref()
            .map_or(Shell::Cmd, ShellProgram::shell)
    }

    fn fence_status(&self) -> FenceStatus {
        self.fence.clone()
    }

    fn exec_with_config(
        &self,
        command: &str,
        config: ExecConfig,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        let command = command.to_string();
        Box::pin(async move { self.exec_inner(command, config, None).await })
    }

    fn exec_streaming_captured(
        &self,
        command: &str,
        _lines: Arc<AtomicI64>,
        config: ExecConfig,
        receiver: Option<Arc<dyn RawOutputReceiver>>,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        let command = command.to_string();
        Box::pin(async move { self.exec_inner(command, config, receiver).await })
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
        self.dirs.add_write(path).map(|_| ())
    }

    fn add_reading_dir(&self, path: &str) -> Result<(), SandboxError> {
        self.dirs.add_read(path).map(|_| ())
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

impl Containment for WindowsJobSession {
    // The job object fences CPU, memory and tree lifetime; it carries no
    // path primitive, so the kernel cannot back a writable-roots claim.
    // Coverage stays Unfenced honestly: the resolver admits the runtime
    // grants, but with no kernel path fence every exec asks.
    fn coverage(&self) -> Coverage {
        Coverage::Unfenced
    }

    fn would_block(&self, _effect: SideEffect) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests;
