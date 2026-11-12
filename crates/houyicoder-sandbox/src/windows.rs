//! Windows sandbox session: one kernel job per command. Each exec creates an
//! unnamed job, assigns the child to it, and ends the whole tree when the
//! call finishes, the wall timeout trips, or the capture refuses output — so
//! a grandchild cannot outlive the command that started it. With the enforce
//! feature off, or on another platform, the session is a no-op fence whose
//! methods log the gap.

use crate::output_capture::{CapturedOutput, DRAIN_GRACE, capture, pipe_streams};
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
use houyicoder_resilience::resource_breaker::ResourceBreaker;
use std::env::temp_dir;
#[cfg(all(target_os = "windows", feature = "enforce"))]
use std::ffi::c_void;
use std::fs::{create_dir_all, remove_dir_all};
use std::path::{Path, PathBuf};
use std::process::id;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::process::Child;
use tokio::process::Command;
use tokio::time::timeout;
#[cfg(all(target_os = "windows", feature = "enforce"))]
use windows::Win32::Foundation::HANDLE;
#[cfg(all(target_os = "windows", feature = "enforce"))]
use windows::Win32::System::JobObjects::{
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
};
#[cfg(all(target_os = "windows", feature = "enforce"))]
use windows::Win32::System::Threading::{PROCESS_SET_QUOTA, PROCESS_TERMINATE};

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

/// The creation flag that suppresses the console window a tool command would
/// otherwise allocate, avoiding the conhost flash on every spawn.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

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
// Fence construction. Under windows plus the enforce feature this creates a
// Job Object and applies the extended limits; otherwise it is a no-op (the
// stub session logs the unfenced gap from each method).
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

// ----------------------------------------------------------------------------
// Safe wrappers over the Win32 Job Object FFI. The windows crate exposes
// these calls as unsafe functions (raw pointers and kernel handles), so this
// module is the ONLY place unsafe_code is allowed in the sandbox crate. The
// allow is scoped to this submodule; the rest of the crate keeps the
// workspace deny. Each wrapper takes owned or borrowed safe inputs, performs
// the one FFI call, and returns the windows::core::Result so callers map
// errors without touching unsafe themselves.
//
// The Job Object handle is a process-wide kernel reference that is safe to
// share across threads; the wrappers below do not dereference the pointer
// the handle wraps, only hand it to the kernel.
// ----------------------------------------------------------------------------

#[cfg(all(target_os = "windows", feature = "enforce"))]
#[expect(
    unsafe_code,
    reason = "Windows FFI: Job Object kernel handle + raw pointer calls"
)]
mod ffi {
    use std::ffi::c_void;
    use std::mem::size_of;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOBOBJECTINFOCLASS, JobObjectExtendedLimitInformation, SetInformationJobObject,
        TerminateJobObject,
    };
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_ACCESS_RIGHTS};
    use windows::core::PCWSTR;

    pub fn create_job_object() -> windows::core::Result<HANDLE> {
        // A null name yields an unnamed job — the standard form for a
        // single-owner job that is never opened by name from elsewhere.
        let name: PCWSTR = PCWSTR::null();
        unsafe { CreateJobObjectW(None, name) }
    }

    pub fn set_extended_limits(
        job: HANDLE,
        info: &JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    ) -> windows::core::Result<()> {
        let info_class: JOBOBJECTINFOCLASS = JobObjectExtendedLimitInformation;
        let len = size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32;
        unsafe { SetInformationJobObject(job, info_class, info as *const _ as *const c_void, len) }
    }

    pub fn assign_process(job: HANDLE, child: HANDLE) -> windows::core::Result<()> {
        unsafe { AssignProcessToJobObject(job, child) }
    }

    /// End every process in the job. The kernel walks the whole tree, so a
    /// grandchild that outlived its parent is included.
    pub fn terminate_job(job: HANDLE, exit_code: u32) -> windows::core::Result<()> {
        unsafe { TerminateJobObject(job, exit_code) }
    }

    pub fn open_process(
        access: PROCESS_ACCESS_RIGHTS,
        inherit: bool,
        pid: u32,
    ) -> windows::core::Result<HANDLE> {
        unsafe { OpenProcess(access, inherit, pid) }
    }

    pub fn close_handle(handle: HANDLE) -> windows::core::Result<()> {
        unsafe { CloseHandle(handle) }
    }
}

// ----------------------------------------------------------------------------
// Per-command job. One command, one kernel job: the wall timeout, a cancel,
// or the capture refusing output ends the whole tree at once, because
// TerminateJobObject walks every descendant. The guard closes the handle in
// Drop, so a tree never outlives the call even when the error path forgets.
// ----------------------------------------------------------------------------

/// The kernel job bound to exactly one exec call. The handle is carried as
/// the raw pointer value so the guard moves into async futures without an
/// unsafe impl. Dropping it ends the job: terminate first, then close, so a
/// process still running is killed rather than left to the last-handle rule.
#[cfg(all(target_os = "windows", feature = "enforce"))]
struct CommandJob {
    handle: Option<usize>,
}

#[cfg(all(target_os = "windows", feature = "enforce"))]
impl CommandJob {
    /// Create a job that dies with its last handle, and assign the child to
    /// it. A child that cannot be assigned is killed through its own handle
    /// and the error names the child, because a command the kernel cannot
    /// group is a command the fence cannot bound.
    fn holding(child: &Child) -> Result<Self, String> {
        let job = ffi::create_job_object().map_err(|e| format!("create job object: {e}"))?;
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = Default::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        ffi::set_extended_limits(job, &info).map_err(|e| format!("set job limits: {e}"))?;
        let raw = job.0 as usize;
        let pid = child.id().ok_or_else(|| {
            drop(ffi::close_handle(job));
            "the child exited before it could be grouped".to_string()
        })?;
        // Set quota is what AssignProcessToJobObject needs, and terminate is
        // what TerminateJobObject reaches the child through. The child handle
        // is closed after the assignment; the job holds its own reference.
        let access = PROCESS_SET_QUOTA | PROCESS_TERMINATE;
        let opened = ffi::open_process(access, false, pid)
            .map_err(|e| format!("open child process {pid}: {e}"))?;
        let assigned = ffi::assign_process(job, opened);
        let closed = ffi::close_handle(opened);
        if let Err(e) = assigned {
            drop(ffi::terminate_job(job, 1));
            drop(ffi::close_handle(job));
            drop(closed);
            return Err(format!("assign pid {pid} to job: {e}"));
        }
        drop(closed);
        Ok(Self { handle: Some(raw) })
    }

    /// End the tree now. Used when the wall timeout trips or the capture
    /// refuses output, so the whole command tree stops instead of only the
    /// direct child the wall timeout would reach.
    fn terminate(&mut self) {
        if let Some(raw) = self.handle.take() {
            let job = HANDLE(raw as *mut c_void);
            drop(ffi::terminate_job(job, 1));
            drop(ffi::close_handle(job));
        }
    }

    /// A guard that owns no job, for an exec whose session is not enforced.
    fn none() -> Self {
        Self { handle: None }
    }
}

#[cfg(all(target_os = "windows", feature = "enforce"))]
impl Drop for CommandJob {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[cfg(not(all(target_os = "windows", feature = "enforce")))]
struct CommandJob;

#[cfg(not(all(target_os = "windows", feature = "enforce")))]
impl CommandJob {
    fn none() -> Self {
        Self
    }

    fn terminate(&mut self) {}
}

impl WindowsJobSession {
    /// Run one command, feeding its raw output to a capture when one is
    /// given. The per-command job bounds the tree; the drain feeds the
    /// receiver as bytes arrive, so a capture that refuses a chunk ends the
    /// command before it runs to its wall budget.
    #[expect(clippy::disallowed_methods, reason = "infra spawn, not model-driven")]
    async fn exec_inner(
        &self,
        command: String,
        config: ExecConfig,
        receiver: Option<Arc<dyn RawOutputReceiver>>,
    ) -> Result<ExecResult, SandboxError> {
        let wall = Duration::from_millis(config.wall_timeout_ms);
        let _ = (config.cpu_secs, config.as_bytes, config.nproc);
        if self.fence.is_unfenced() {
            tracing::warn!(
                "sandbox audit: windows job object NOT enforced; running unfenced (wall={}s)",
                config.wall_timeout_ms
            );
        }
        let shell = self.program.clone().unwrap_or(ShellProgram::Cmd);
        let mut cmd = Command::new(shell.executable());
        cmd.args(shell.arguments(&command))
            .current_dir(&self.workspace);
        // A tool command never needs a console window of its own; the flag
        // also avoids the conhost allocation that flashes a black box.
        #[cfg(target_os = "windows")]
        cmd.creation_flags(CREATE_NO_WINDOW);
        pipe_streams(&mut cmd);
        cmd.kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .map_err(|e| SandboxError::SandboxUnavailable(format!("spawn: {e}")))?;
        // The job exists only when the fence is enforced: an unfenced run
        // holds the wall timeout and kill_on_drop alone, and a session whose
        // fence failed says so once at startup rather than per command.
        let mut job = if matches!(self.fence, FenceStatus::Enforced) {
            CommandJob::holding(&child).map_err(|e| {
                // A child the kernel cannot group is a command the fence
                // cannot bound, so it is killed before the error returns and
                // the caller learns the command never ran fenced.
                drop(child.start_kill());
                SandboxError::SandboxUnavailable(format!("job fence: {e}"))
            })?
        } else {
            CommandJob::none()
        };
        // The bytes land in collected as they arrive, so a wall-clock stop
        // still reports what the command printed before the job killed the
        // tree.
        let collected = CapturedOutput::default();
        let outcome = timeout(
            wall,
            capture(child, None, &collected, DRAIN_GRACE, receiver.as_deref()),
        )
        .await;
        // The tree ends with the call in every outcome: the timeout kills the
        // whole job rather than the direct child only, and the guard drops on
        // the return path even when the capture itself errored.
        if matches!(outcome, Err(ref _elapsed)) {
            job.terminate();
        }
        drop(job);
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
