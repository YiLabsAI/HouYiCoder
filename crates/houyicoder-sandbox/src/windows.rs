//! Windows sandbox session. Two mutually exclusive cfg paths:
//!
//! - windows plus the enforce feature: a real Job Object fence — a
//!   kernel-level process group with enforceable resource limits. Each exec
//!   assigns the spawned child to the job so the fence covers the whole
//!   child tree; Drop closes the handle and KILL_ON_JOB_CLOSE reaps
//!   survivors.
//! - otherwise (non-Windows, or the enforce feature off): a no-op fence.
//!   Each operation logs an unfenced audit line via tracing so a log
//!   scanner can see the gap; the user-visible notice fires once at startup
//!   via FenceStatus (see composition root).
//!
//! Why a Job Object and not a restricted token or AppContainer: a Job Object
//! gives the three fences this backend needs in one kernel primitive — tree
//! grouping, CPU/memory caps, and a per-process kill surface. A restricted
//! token is stronger on the security side but needs unsafe FFI, which the
//! workspace lint denies; the Job Object path keeps every unsafe call inside
//! the ffi submodule below so the rest of the module stays safe.
//!
//! Resource vocabulary: the macOS backend fences CPU seconds and address
//! space; this backend does the same. Windows extended limits express CPU as
//! a per-process user-time limit in 100-nanosecond units, so cpu_secs maps
//! directly; memory is a process and job commit cap in bytes.
//!
//! Graceful degradation: if Job Object creation or limit application fails,
//! the session stays a no-op fence — exec runs unfenced and the
//! application-level path resolver remains the only boundary.

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

/// A Windows sandbox session. The workspace is the user's project dir (Guarded
/// mode) or a temp dir. Drop leaves the workspace untouched when not owned.
/// Under windows plus the enforce feature, construction creates a Job Object
/// and applies the resource fence; otherwise the session is a no-op fence.
pub struct WindowsJobSession {
    workspace: PathBuf,
    owned: bool,
    /// Kernel job-object handle, carried as the raw pointer value so the
    /// struct stays Send + Sync without an unsafe impl. The handle is a
    /// process-wide reference to a kernel object that is safe to share
    /// across threads. None when construction degraded to a no-op fence.
    job_handle: Option<usize>,
    fence: FenceStatus,
    /// Runtime directory grants. The job object carries no path primitive,
    /// so these widen the resolver only: the kernel fence stays limited to
    /// resources and the coverage answer remains honest about that.
    dirs: RuntimeDirs,
}

impl WindowsJobSession {
    /// Create a session rooted at the user's project dir. The dir is
    /// canonicalized through dunce so the UNC prefix std canonicalize yields
    /// on Windows does not break downstream string ops. The user's directory
    /// is never removed on Drop. Construction creates the Job Object and
    /// applies the resource fence (real under windows plus enforce, no-op
    /// otherwise).
    pub fn new_in_cwd(cwd: &Path) -> Result<Self, SandboxError> {
        let workspace = dunce::canonicalize(cwd)
            .map_err(|e| SandboxError::Io(format!("cwd canonicalize: {e}")))?;
        let (job_handle, fence) = apply_fence();
        Ok(Self {
            workspace,
            owned: false,
            job_handle,
            fence,
            dirs: RuntimeDirs::default(),
        })
    }

    /// Create a session rooted at a fresh temp dir this session owns. Drop
    /// removes it. Used by tests and as a fallback when no cwd is available.
    pub fn new() -> Result<Self, SandboxError> {
        let mut workspace = std::env::temp_dir();
        workspace.push(format!(
            "houyicoder-sandbox-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&workspace)?;
        let (job_handle, fence) = apply_fence();
        Ok(Self {
            workspace,
            owned: true,
            job_handle,
            fence,
            dirs: RuntimeDirs::default(),
        })
    }

    /// No-op: the job applies its own CPU and memory caps in the kernel.
    /// Present for PlatformSession parity.
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
        // Close the kernel job handle first. KILL_ON_JOB_CLOSE trips here if
        // any child of the job is still alive, reaping the whole tree before
        // the workspace is touched. Best-effort; never panic in Drop.
        if let Some(raw) = self.job_handle.take() {
            close_job_handle(raw);
        }
        if self.owned {
            let _result = std::fs::remove_dir_all(&self.workspace);
        }
    }
}

// ----------------------------------------------------------------------------
// Fence construction. Under windows plus the enforce feature this creates a
// Job Object and applies the extended limits; otherwise it is a no-op (the
// stub session logs the unfenced gap from each method).
// ----------------------------------------------------------------------------

#[cfg(all(target_os = "windows", feature = "enforce"))]
fn apply_fence() -> (Option<usize>, FenceStatus) {
    use windows::Win32::System::JobObjects::{
        JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOB_OBJECT_LIMIT_PROCESS_TIME,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    };

    // Test-suite escape hatch, debug builds only (see the linux backend). The
    // job caps CPU and memory for assigned children and reaps the tree on handle
    // close, so a test spawning helpers can be killed by a fence it never asked
    // for. One env var covers every platform fence.
    #[cfg(debug_assertions)]
    if std::env::var("HOUYICODER_SANDBOX_NO_ENFORCE").is_ok_and(|v| v == "1") {
        tracing::warn!(
            "sandbox audit: job object enforcement skipped via HOUYICODER_SANDBOX_NO_ENFORCE (test builds only); running unfenced"
        );
        return (None, FenceStatus::Unavailable);
    }

    let result = (|| {
        let job = ffi::create_job_object().map_err(|e| format!("create job object: {e}"))?;
        let cfg = ExecConfig::default();
        // Per-process user-time cap in 100-nanosecond units. A CPU-second is
        // 10_000_000 such units. Reaching the cap terminates the process
        // outright; there is no soft signal phase. The caps are fixed here at
        // session construction, so a per-call ExecConfig override of cpu_secs
        // or as_bytes does not retune the job — only the wall timeout is
        // per-call on this backend.
        let cpu_100ns = (cfg.cpu_secs as i64).saturating_mul(10_000_000);
        let mem = cfg.as_bytes as usize;
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = Default::default();
        info.BasicLimitInformation.PerProcessUserTimeLimit = cpu_100ns;
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            | JOB_OBJECT_LIMIT_PROCESS_TIME
            | JOB_OBJECT_LIMIT_PROCESS_MEMORY
            | JOB_OBJECT_LIMIT_JOB_MEMORY;
        info.ProcessMemoryLimit = mem;
        info.JobMemoryLimit = mem;
        ffi::set_extended_limits(job, &info).map_err(|e| format!("set job limits: {e}"))?;
        // Carry the handle as the raw pointer value so the session stays
        // Send + Sync without an unsafe impl. Reconstructing the wrapper at
        // the call site is a safe struct construction (pub field), not a
        // pointer dereference.
        Ok::<usize, String>(job.0 as usize)
    })();
    match result {
        Ok(raw) => (Some(raw), FenceStatus::Enforced),
        Err(e) => {
            tracing::warn!("sandbox audit: windows job object apply failed: {e}; running unfenced");
            (None, FenceStatus::Failed(e))
        }
    }
}

#[cfg(not(all(target_os = "windows", feature = "enforce")))]
fn apply_fence() -> (Option<usize>, FenceStatus) {
    // No-op: the stub session methods emit their own unfenced audit lines.
    (None, FenceStatus::Unavailable)
}

#[cfg(all(target_os = "windows", feature = "enforce"))]
fn close_job_handle(raw: usize) {
    use windows::Win32::Foundation::HANDLE;
    let handle = HANDLE(raw as *mut std::ffi::c_void);
    // Best-effort; never propagate a kernel close failure into Drop panic.
    drop(ffi::close_handle(handle));
}

#[cfg(not(all(target_os = "windows", feature = "enforce")))]
fn close_job_handle(_raw: usize) {}

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
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOBOBJECTINFOCLASS, SetInformationJobObject,
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
        let info_class: JOBOBJECTINFOCLASS =
            windows::Win32::System::JobObjects::JobObjectExtendedLimitInformation;
        let len = std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32;
        unsafe {
            SetInformationJobObject(
                job,
                info_class,
                info as *const _ as *const std::ffi::c_void,
                len,
            )
        }
    }

    pub fn assign_process(job: HANDLE, child: HANDLE) -> windows::core::Result<()> {
        unsafe { AssignProcessToJobObject(job, child) }
    }

    pub fn open_process(
        access: PROCESS_ACCESS_RIGHTS,
        inherit: bool,
        pid: u32,
    ) -> windows::core::Result<HANDLE> {
        unsafe { OpenProcess(access, inherit, pid) }
    }

    pub fn close_handle(handle: HANDLE) -> windows::core::Result<()> {
        unsafe { windows::Win32::Foundation::CloseHandle(handle) }
    }

    pub fn query_extended_limits(
        job: HANDLE,
    ) -> windows::core::Result<JOBOBJECT_EXTENDED_LIMIT_INFORMATION> {
        use windows::Win32::System::JobObjects::{
            JobObjectExtendedLimitInformation, QueryInformationJobObject,
        };
        let info_class: JOBOBJECTINFOCLASS = JobObjectExtendedLimitInformation;
        // zeroed is safe here: the struct is plain old data and the kernel
        // overwrites every field on a successful query.
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        let len = std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32;
        unsafe {
            QueryInformationJobObject(
                Some(job),
                info_class,
                &mut info as *mut _ as *mut std::ffi::c_void,
                len,
                None,
            )
        }?;
        Ok(info)
    }
}

/// A snapshot of the limits the kernel reports on the job object. Read back
/// through QueryInformationJobObject so a test or smoke binary can verify the
/// fence landed on the kernel object, not just on the in-memory config.
#[cfg(all(target_os = "windows", feature = "enforce"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobLimits {
    /// True when KILL_ON_JOB_CLOSE is set: closing the job handle reaps the
    /// whole child tree, the orphan-process fix.
    pub kill_on_close: bool,
    /// Per-process user-time cap in 100-nanosecond units.
    pub cpu_100ns: i64,
    /// Per-process commit cap in bytes.
    pub process_memory: usize,
    /// Per-job commit cap in bytes.
    pub job_memory: usize,
}

#[cfg(all(target_os = "windows", feature = "enforce"))]
impl WindowsJobSession {
    /// Query the limits the kernel currently reports on the job. Returns None
    /// when construction degraded to a no-op fence (no job handle). Used by
    /// the smoke binary to assert the fence landed on the kernel object.
    pub fn job_limits(&self) -> Option<Result<JobLimits, String>> {
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::JobObjects::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let raw = self.job_handle?;
        let job = HANDLE(raw as *mut std::ffi::c_void);
        Some(
            ffi::query_extended_limits(job)
                .map(|info| JobLimits {
                    kill_on_close: (info.BasicLimitInformation.LimitFlags
                        & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE)
                        == JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                    cpu_100ns: info.BasicLimitInformation.PerProcessUserTimeLimit,
                    process_memory: info.ProcessMemoryLimit,
                    job_memory: info.JobMemoryLimit,
                })
                .map_err(|e| format!("query job limits: {e}")),
        )
    }
}

// ----------------------------------------------------------------------------
// SandboxSession impl. One impl for both fence outcomes: the job handle is
// an Option, so a degraded session runs the same spawn path with an audit
// line and no assignment, and the resolver and grant behavior are identical
// whether or not the kernel fence engaged.
// ----------------------------------------------------------------------------

impl SandboxSession for WindowsJobSession {
    fn fence_status(&self) -> FenceStatus {
        self.fence.clone()
    }

    #[expect(clippy::disallowed_methods, reason = "infra spawn, not model-driven")]
    fn exec_with_config(
        &self,
        command: &str,
        config: ExecConfig,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        let cwd = self.workspace.clone();
        let command = command.to_string();
        let wall = std::time::Duration::from_millis(config.wall_timeout_ms);
        let job_raw = self.job_handle;
        Box::pin(async move {
            let _ = (config.cpu_secs, config.as_bytes, config.nproc);
            if job_raw.is_none() {
                tracing::warn!(
                    "sandbox audit: windows job object NOT enforced; running unfenced (wall={}s)",
                    config.wall_timeout_ms
                );
            }
            let mut cmd = tokio::process::Command::new("cmd");
            cmd.arg("/C")
                .arg(&command)
                .current_dir(&cwd)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true);
            let child = cmd
                .spawn()
                .map_err(|e| SandboxError::SandboxUnavailable(format!("spawn: {e}")))?;
            assign_if_fenced(job_raw, &child);
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

/// Hand the spawned child to the job so the kernel fence (CPU/memory caps
/// plus KILL_ON_JOB_CLOSE tree teardown) covers the child and every
/// descendant. Best-effort: a failed assignment leaves the child running
/// under the wall-timeout plus kill_on_drop only, with an audit line so the
/// gap is visible.
#[cfg(all(target_os = "windows", feature = "enforce"))]
fn assign_if_fenced(job_raw: Option<usize>, child: &tokio::process::Child) {
    let Some(raw) = job_raw else { return };
    let Some(pid) = child.id() else { return };
    if let Err(e) = assign_child_to_job(raw, pid) {
        tracing::warn!("sandbox audit: assign child to job failed: {e}");
    }
}

#[cfg(not(all(target_os = "windows", feature = "enforce")))]
fn assign_if_fenced(_job_raw: Option<usize>, _child: &tokio::process::Child) {}

#[cfg(all(target_os = "windows", feature = "enforce"))]
fn assign_child_to_job(job_raw: usize, pid: u32) -> Result<(), String> {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Threading::{PROCESS_SET_QUOTA, PROCESS_TERMINATE};

    // Open the child by pid with the access rights AssignProcessToJobObject
    // needs (PROCESS_SET_QUOTA) plus PROCESS_TERMINATE so a future
    // TerminateJobObject path can kill the child through the job. The handle
    // is closed below after the assignment; the job keeps its own reference.
    let access = PROCESS_SET_QUOTA | PROCESS_TERMINATE;
    let child = ffi::open_process(access, false, pid)
        .map_err(|e| format!("open child process {pid}: {e}"))?;
    let job = HANDLE(job_raw as *mut std::ffi::c_void);
    let result =
        ffi::assign_process(job, child).map_err(|e| format!("assign pid {pid} to job: {e}"));
    drop(ffi::close_handle(child));
    result
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
mod tests {
    // Construction creates a Job Object but never restricts this process --
    // only children assigned to it -- so these tests run under both fence
    // outcomes (see the jobobject_smoke example for the kernel-side check).

    use super::*;

    fn unique_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "houyicoder-windows-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ))
    }

    #[test]
    fn test_session_constructs_workspace() {
        let session = WindowsJobSession::new().expect("session");
        assert!(session.workspace.exists());
        assert!(session.workspace.is_dir());
    }

    #[test]
    fn test_absolute_path_rejected() {
        let session = WindowsJobSession::new().expect("session");
        let resolved = session.resolve("C:/secret");
        assert!(matches!(resolved, Err(SandboxError::PathTraversal(_))));
    }

    #[test]
    fn test_granted_dir_admitted() {
        let session = WindowsJobSession::new().expect("session");
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
    fn test_coverage_unfenced() {
        // The job object carries no path primitive, so no grant set can back
        // a writable-roots claim; the answer stays Unfenced even with grants,
        // and the gate keeps asking for consent.
        let session = WindowsJobSession::new().expect("session");
        assert!(matches!(session.coverage(), Coverage::Unfenced));
        let granted = unique_dir("coverage");
        std::fs::create_dir_all(&granted).expect("create granted dir");
        let granted_str = granted.to_str().expect("granted str");
        session.add_working_dir(granted_str).expect("grant");
        assert!(matches!(session.coverage(), Coverage::Unfenced));
        session.remove_working_dir(granted_str);
        let _cleanup = std::fs::remove_dir_all(&granted);
    }
}
