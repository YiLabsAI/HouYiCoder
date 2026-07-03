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

/// Per-command resource fence config. Defaults are industrial-grade: 30s CPU
/// (kernel SIGXCPU, not app-polled), 2GB address space, 256 processes (fork-
/// bomb backstop), 120s wall-clock. The fence kills the whole process tree
/// on any breach, not just the direct child (prevents orphan processes where
/// grandchildren survive and burn CPU for minutes).
#[derive(Debug, Clone, Copy)]
pub struct ExecConfig {
    /// CPU seconds before SIGXCPU (soft) then SIGKILL (hard). Kernel-enforced.
    pub cpu_secs: u64,
    /// Max address space bytes (RLIMIT_AS).
    pub as_bytes: u64,
    /// Max processes the user may spawn (RLIMIT_NPROC) -- fork-bomb backstop.
    pub nproc: u64,
    /// Wall-clock seconds before the tree is killpg'd.
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
