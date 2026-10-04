//! Sandbox payload types. These cross the port boundary (the SandboxSession
//! trait in ports references them), so they live in the foundation crate
//! alongside the other domain vocabulary. The trait stays in ports; the
//! concrete MacSeatbeltSession impl stays in the sandbox crate; the types
//! are shared here so neither ports nor the engine depends on the sandbox
//! impl crate.

use std::fmt;

/// Failures a sandbox session can report. The kind tag gives observability a
/// stable lower-case label without leaking the enum shape. Carries the
/// underlying detail as a string so callers log it but cannot accidentally
/// match on it instead of the enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxError {
    Io(String),
    Unsupported(String),
    Timeout(String),
    ResourceLimitExceeded(String),
    NotFound(String),
    PathTraversal(String),
    InvalidConfig(String),
    SandboxUnavailable(String),
    BreakerOpen(String),
}

impl SandboxError {
    /// A stable lowercase kind string for logs and observability. Does not
    /// leak the enum shape so callers cannot accidentally match on it
    /// instead of the enum.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Io(_) => "io",
            Self::Unsupported(_) => "unsupported",
            Self::Timeout(_) => "timeout",
            Self::ResourceLimitExceeded(_) => "resource_limit_exceeded",
            Self::NotFound(_) => "not_found",
            Self::PathTraversal(_) => "path_traversal",
            Self::InvalidConfig(_) => "invalid_config",
            Self::SandboxUnavailable(_) => "sandbox_unavailable",
            Self::BreakerOpen(_) => "breaker_open",
        }
    }
}

impl fmt::Display for SandboxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(m) => write!(f, "sandbox io error: {m}"),
            Self::Unsupported(m) => write!(f, "sandbox unsupported: {m}"),
            Self::Timeout(m) => write!(f, "sandbox timeout: {m}"),
            Self::ResourceLimitExceeded(m) => {
                write!(f, "sandbox resource limit exceeded: {m}")
            }
            Self::NotFound(m) => write!(f, "sandbox not found: {m}"),
            Self::PathTraversal(m) => write!(f, "sandbox path traversal: {m}"),
            Self::InvalidConfig(m) => write!(f, "sandbox invalid config: {m}"),
            Self::SandboxUnavailable(m) => write!(f, "sandbox unavailable: {m}"),
            Self::BreakerOpen(m) => write!(f, "sandbox breaker open (cool-down): {m}"),
        }
    }
}

impl std::error::Error for SandboxError {}

impl From<std::io::Error> for SandboxError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

/// The result of running one command in a sandbox session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    /// The process exit code. None when the process was killed by a signal.
    pub exit_code: Option<i32>,
}

impl ExecResult {
    /// True when the command exited 0.
    pub fn is_success(&self) -> bool {
        self.exit_code == Some(0)
    }
}

/// Per-command resource fence config. Which fields are honored differs by
/// backend: the Windows job object enforces cpu_secs and as_bytes on the
/// spawned tree; macOS and Linux enforce wall_timeout_ms only, killing the
/// whole process tree on expiry (never just the direct child, so no orphan
/// grandchild survives and burns CPU). Neither unix backend applies per-spawn
/// rlimits: macOS has no safe in-child primitive, and the Linux rlimits are
/// per real user ID or per virtual address space rather than per tree, so
/// arming them breaks ordinary shells and compilers instead of budgeting the
/// fenced tree. The Windows caps are fixed at session construction from the
/// default config, so a per-call override of cpu_secs or as_bytes does not
/// retune them; wall_timeout_ms is honored per call on every backend. nproc
/// is reserved for a future per-tree process budget.
#[derive(Debug, Clone, Copy)]
pub struct ExecConfig {
    /// CPU seconds budget. Enforced by the Windows job object; macOS and
    /// Linux rely on wall_timeout_ms.
    pub cpu_secs: u64,
    /// Memory cap in bytes. Enforced by the Windows job object as a commit
    /// charge cap; not applied on macOS or Linux.
    pub as_bytes: u64,
    /// Per-tree process budget. Not enforced on any backend yet; kept as the
    /// configuration point for a process-count fence.
    pub nproc: u64,
    /// Wall-clock milliseconds before the tree is group-killed.
    pub wall_timeout_ms: u64,
}

impl Default for ExecConfig {
    fn default() -> Self {
        Self {
            cpu_secs: 30,
            as_bytes: 2 * 1024 * 1024 * 1024,
            nproc: 256,
            wall_timeout_ms: 120000,
        }
    }
}

/// One directory entry (name + whether it is a directory).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
}
