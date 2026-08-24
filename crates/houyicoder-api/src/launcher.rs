//! The spawn chokepoint port: the single contract every process spawn in the
//! engine routes through. Spawning a process is a security-sensitive operation
//! (a tool can shell out, a guest can exec, a verify gate runs commands), so
//! the trait is the one place that applies a resource fence, a wrapper, and an
//! audit log. Concrete launchers (a kernel-fenced sandbox launcher, a wrapper
//! launcher) live in the sandbox and service layers; the trait descends to the
//! ports layer so neither the engine nor the permission layer depends on an impl
//! crate to spawn.
//!
//! The default launcher implementation (StdProcessLauncher) lives in the
//! std_launcher submodule and wraps std Command (sync output capture) and
//! tokio Command (async child with kill-on-drop) behind the trait. The sandbox
//! layer's launcher (a seatbelt-fenced launcher) is the stronger impl; this
//! default impl is the fallback when no sandbox is wired.

#[path = "std_launcher.rs"]
mod std_launcher;

use houyicoder_async::PFut;
use std::path::PathBuf;

pub use std_launcher::StdProcessLauncher;

/// Per-stream stdio disposition for a spawn. Inherit passes the parent stdio
/// through; Piped captures output for the caller; Null discards it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StdioMode {
    /// The child inherits the parent stdio stream.
    #[default]
    Inherit,
    /// The child stdio stream is piped so the launcher captures its output.
    Piped,
    /// The child stdio stream is connected to /dev/null.
    Null,
}

/// The stdio configuration for a spawn: one mode per stream so a caller can
/// capture stdout while inheriting stderr, or any combination.
#[derive(Debug, Clone, Copy, Default)]
pub struct StdioConfig {
    /// stdin disposition.
    pub stdin: StdioMode,
    /// stdout disposition.
    pub stdout: StdioMode,
    /// stderr disposition.
    pub stderr: StdioMode,
}

impl StdioConfig {
    /// Pipe stdout and stderr so the caller can capture both; null stdin.
    pub fn piped_output() -> Self {
        Self {
            stdin: StdioMode::Null,
            stdout: StdioMode::Piped,
            stderr: StdioMode::Piped,
        }
    }

    /// Pipe stdin, stdout, and stderr so the caller can drive a long-lived
    /// child interactively (a line-protocol subprocess). Used with a spawn
    /// request marked interactive so the launcher hands the live pipe handles
    /// back instead of capturing output on exit.
    pub fn piped_io() -> Self {
        Self {
            stdin: StdioMode::Piped,
            stdout: StdioMode::Piped,
            stderr: StdioMode::Piped,
        }
    }
}

/// Per-command kernel resource fence. Mirrors the fence vocabulary the sandbox
/// already enforces (CPU seconds, address space, process count, wall-clock
/// timeout); a launcher applies it via setrlimit + a process-group tree-kill on
/// breach. A fence is optional on a spawn: a trusted inner command (a git
/// worktree op) may spawn with no fence, while a tool exec always carries one.
#[derive(Debug, Clone, Copy)]
pub struct FenceConfig {
    /// CPU seconds before the kernel sends SIGXCPU then SIGKILL.
    pub cpu_secs: u64,
    /// Max address space bytes (RLIMIT_AS).
    pub as_bytes: u64,
    /// Max processes the user may spawn (RLIMIT_NPROC) — a fork-bomb backstop.
    pub nproc: u64,
    /// Wall-clock milliseconds before the whole process group is killpg'd.
    pub wall_timeout_ms: u64,
}

impl Default for FenceConfig {
    fn default() -> Self {
        Self {
            cpu_secs: 30,
            as_bytes: 2 * 1024 * 1024 * 1024,
            nproc: 256,
            wall_timeout_ms: 120000,
        }
    }
}

/// Configuration for a wrapper command that wraps the spawned program (an
/// enterprise exec wrapper, a seatbelt launcher, a ptrace tracer). The wrapper
/// program runs the requested command as its argv; the launcher pipes the
/// request through it when set.
#[derive(Debug, Clone)]
pub struct WrapperConfig {
    /// The wrapper program path.
    pub program: String,
    /// Extra argv passed to the wrapper before the wrapped command.
    pub args: Vec<String>,
}

/// Configuration for a spawn. Carries the program + argv, the workspace the
/// spawn runs in, a seatbelt profile name (macOS sandbox-exec is policy, not
/// just a fence; Linux Landlock and Windows Job Object carry their own
/// cfg-gated configuration the launcher reads), and the per-stream stdio
/// disposition.
#[derive(Debug, Clone, Default)]
pub struct SpawnRequest {
    /// The program to run.
    pub program: String,
    /// Argv after the program.
    pub args: Vec<String>,
    /// The workspace root the spawn runs in (cwd). None for a process with no
    /// workspace affinity.
    pub workspace: Option<PathBuf>,
    /// A seatbelt profile name the launcher passes to a platform sandbox-exec.
    pub seatbelt_profile: Option<String>,
    /// Per-stream stdio disposition. Defaults to inherit so a spawn without
    /// explicit configuration behaves like a plain exec.
    pub stdio: StdioConfig,
    /// True to spawn an interactive piped child: the launcher hands the live
    /// stdin/stdout/stderr pipe handles back to the caller instead of
    /// capturing output on exit. Used for a long-lived subprocess the caller
    /// speaks a line protocol with. The piped streams in stdio must be set to
    /// Piped for the corresponding handle to come back; an unset stream stays
    /// None on the returned pipes.
    pub interactive: bool,
}

impl SpawnRequest {
    /// Construct a spawn request for the given program with empty args, no
    /// workspace, and inherited stdio.
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            ..Default::default()
        }
    }

    /// Set the argv after the program.
    pub fn with_args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    /// Set the workspace (cwd) for the spawn.
    pub fn with_workspace(mut self, path: impl Into<PathBuf>) -> Self {
        self.workspace = Some(path.into());
        self
    }

    /// Pipe stdout and stderr so the caller can capture both via the child
    /// wait future. stdin is set to null.
    pub fn piped_output(mut self) -> Self {
        self.stdio = StdioConfig::piped_output();
        self
    }

    /// Pipe stdin, stdout, and stderr for an interactive long-lived child and
    /// mark the request interactive so the launcher hands the live pipe
    /// handles back. Used by a caller that drives a line-protocol subprocess.
    pub fn interactive(mut self) -> Self {
        self.interactive = true;
        self.stdio = StdioConfig::piped_io();
        self
    }
}

/// Composable spawn policy. A struct (not a mutex enum) so decorations stack:
/// audit stacks on a fence, a wrapper stacks on a fence, any combination is
/// expressible. The chokepoint's audit property is exactly that it applies to
/// EVERY spawn regardless of other policy, which a mutex enum could not express.
///
/// non_exhaustive so a future policy field (a cgroup, a network namespace) lands
/// without reworking every construction site; callers construct via Default +
/// the builders, never via a literal.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct SpawnPolicy {
    /// Kernel resource fence. None for a trusted spawn that needs no fence.
    pub fence: Option<FenceConfig>,
    /// A wrapper command the spawn pipes through. None for a direct spawn.
    pub wrapper: Option<WrapperConfig>,
    /// True to emit a spawn audit log entry. Stacks on any policy.
    pub audit: bool,
}

impl SpawnPolicy {
    /// Attach a default kernel fence (the chokepoint's standard resource caps).
    pub fn sandboxed(mut self) -> Self {
        self.fence = Some(FenceConfig::default());
        self
    }

    /// Attach an explicit fence configuration.
    pub fn with_fence(mut self, fence: FenceConfig) -> Self {
        self.fence = Some(fence);
        self
    }

    /// Attach a wrapper command the spawn pipes through.
    pub fn with_wrapper(mut self, wrapper: WrapperConfig) -> Self {
        self.wrapper = Some(wrapper);
        self
    }

    /// Turn on spawn audit logging. Stacks on any other policy.
    pub fn audited(mut self) -> Self {
        self.audit = true;
        self
    }
}

/// The exit outcome of a spawned process. stdout and stderr are Some when the
/// spawn was configured with piped output; None when the streams were
/// inherited or null.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LauncherExit {
    /// The process exit code. None when the process was killed by a signal.
    pub exit_code: Option<i32>,
    /// Captured stdout, present only when the spawn piped stdout.
    pub stdout: Option<String>,
    /// Captured stderr, present only when the spawn piped stderr.
    pub stderr: Option<String>,
}

/// Failures a launcher can report. Kept as an enum so callers branch on recovery
/// (retry on BreakerOpen cool-down, abort on DeniedByPolicy, surface Io).
#[derive(Debug)]
pub enum SpawnError {
    /// An underlying spawn I/O failure (exec failed, pipe setup failed).
    Io(String),
    /// The spawn policy refused the spawn (a breaker tripped, a deny rule).
    DeniedByPolicy(String),
    /// This launcher does not implement the requested policy (a Linux fence on
    /// a launcher without cgroup support).
    Unsupported(String),
    /// The aggregate resource breaker is open (cool-down after an overrun).
    BreakerOpen(String),
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(msg) => write!(f, "spawn I/O failure: {msg}"),
            Self::DeniedByPolicy(msg) => write!(f, "spawn denied by policy: {msg}"),
            Self::Unsupported(msg) => write!(f, "spawn policy unsupported: {msg}"),
            Self::BreakerOpen(msg) => write!(f, "spawn breaker open (cool-down): {msg}"),
        }
    }
}

impl std::error::Error for SpawnError {}

/// Live stdio pipe handles handed back to the caller when a spawn request is
/// marked interactive. Each handle is present only when the corresponding
/// stream was Piped on the request; an inherited or nulled stream is None.
/// The handles are boxed std traits so the launcher does not leak a concrete
/// runtime child type; a caller that wants async I/O wraps the blocking call
/// in a runtime-blocking task.
pub struct StdioPipes {
    /// The child's stdin, present when the request piped stdin.
    pub stdin: Option<Box<dyn std::io::Write + Send>>,
    /// The child's stdout, present when the request piped stdout.
    pub stdout: Option<Box<dyn std::io::Read + Send>>,
    /// The child's stderr, present when the request piped stderr.
    pub stderr: Option<Box<dyn std::io::Read + Send>>,
}

/// A handle to a spawned process. The launcher returns it from a synchronous
/// spawn; the caller awaits the child's exit through wait(). The wait future is
/// pre-boxed by the launcher so the handle stays Send + 'static without this
/// ports crate depending on a concrete async runtime child type.
///
/// For an interactive spawn (a long-lived child with live stdio handles the
/// caller drives), the pipes field carries the stdin/stdout/stderr handles
/// the caller writes to and reads from for the session; the wait future is
/// not normally awaited in that mode (the caller drops the handle to trigger
/// the on-drop kill).
pub struct LauncherChild {
    /// The spawned process id, if one was minted.
    pub id: Option<u32>,
    /// Live stdio handles for an interactive spawn; None for a capture-on-exit
    /// spawn. Take it out with take_pipes before awaiting wait.
    pub pipes: Option<StdioPipes>,
    waiter: PFut<'static, Result<LauncherExit, SpawnError>>,
}

impl LauncherChild {
    /// Construct a child handle from its pid, pipes, and a pre-boxed wait
    /// future. The concrete launcher (in the sandbox or service layer) builds
    /// the future by capturing the real async-runtime child it spawned.
    pub fn new(id: Option<u32>, waiter: PFut<'static, Result<LauncherExit, SpawnError>>) -> Self {
        Self {
            id,
            pipes: None,
            waiter,
        }
    }

    /// Construct a child handle carrying live stdio pipes for an interactive
    /// spawn. The wait future is typically a no-op (the on-drop path of the
    /// captured child handles the kill); the caller drives the pipes directly.
    pub fn with_pipes(
        id: Option<u32>,
        pipes: StdioPipes,
        waiter: PFut<'static, Result<LauncherExit, SpawnError>>,
    ) -> Self {
        Self {
            id,
            pipes: Some(pipes),
            waiter,
        }
    }

    /// Take the live stdio pipes out of the handle, leaving pipes None. The
    /// caller owns the pipes for the session; the handle retains the wait
    /// future and id. Returns None for a non-interactive spawn.
    pub fn take_pipes(&mut self) -> Option<StdioPipes> {
        self.pipes.take()
    }

    /// Await the child's exit. Consumes the handle.
    ///
    /// Convention, not guarantee: on the capture path the launcher resolves
    /// the exit before returning the handle, so this future is already settled
    /// and a block_on is a single poll. A future launcher whose wait is not
    /// pre-resolved must offer a non-blocking resolution path -- block_on on
    /// an unresolved future parks the calling thread, which starves a runtime
    /// if the caller is on a worker.
    pub fn wait(self) -> PFut<'static, Result<LauncherExit, SpawnError>> {
        self.waiter
    }
}

/// The spawn chokepoint. Synchronous spawn returns a child handle; the child's
/// exit is awaited separately. Object-safe so the composition root holds a
/// single launcher and a kernel-fenced or wrapper launcher swaps behind it.
///
/// The chokepoint applies the policy: fence (process group + setrlimit +
/// tree-kill), wrapper (pipe the command through a wrapper program), audit (log
/// the spawn). A concrete launcher without a kernel fence returns Unsupported
/// for a policy that requests one.
pub trait ProcessLauncher: Send + Sync {
    /// Spawn a process under the given policy. Returns the child handle on
    /// success; the caller awaits exit through the handle.
    fn spawn(&self, req: SpawnRequest, policy: SpawnPolicy) -> Result<LauncherChild, SpawnError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The trait must support runtime dispatch.
    #[test]
    fn test_trait_is_object_safe() {
        let _launcher: Box<dyn ProcessLauncher> = Box::new(Stub);
    }

    struct Stub;
    impl ProcessLauncher for Stub {
        fn spawn(
            &self,
            _req: SpawnRequest,
            _policy: SpawnPolicy,
        ) -> Result<LauncherChild, SpawnError> {
            Ok(LauncherChild::new(
                None,
                Box::pin(async move {
                    Ok(LauncherExit {
                        exit_code: Some(0),
                        stdout: None,
                        stderr: None,
                    })
                }),
            ))
        }
    }

    #[test]
    fn test_policy_builders_stack() {
        // A sandboxed + audited spawn composes; a mutex enum could not express
        // the stack.
        let policy = SpawnPolicy::default().sandboxed().audited();
        assert!(policy.fence.is_some());
        assert!(policy.audit);
    }

    #[test]
    fn test_default_policy_is_passthrough() {
        let policy = SpawnPolicy::default();
        assert!(policy.fence.is_none());
        assert!(policy.wrapper.is_none());
        assert!(!policy.audit);
    }

    #[test]
    fn test_fence_default_is_industrial() {
        let f = FenceConfig::default();
        assert_eq!(f.cpu_secs, 30);
        assert_eq!(f.nproc, 256);
        assert_eq!(f.wall_timeout_ms, 120000);
    }

    #[test]
    fn test_stdio_config_piped_output() {
        let s = StdioConfig::piped_output();
        assert_eq!(s.stdin, StdioMode::Null);
        assert_eq!(s.stdout, StdioMode::Piped);
        assert_eq!(s.stderr, StdioMode::Piped);
    }

    #[test]
    fn test_spawn_request_builder() {
        let req = SpawnRequest::new("make")
            .with_args(["check"])
            .with_workspace("/tmp")
            .piped_output();
        assert_eq!(req.program, "make");
        assert_eq!(req.args, vec!["check"]);
        assert_eq!(req.workspace.as_deref(), Some(std::path::Path::new("/tmp")));
        assert_eq!(req.stdio.stdout, StdioMode::Piped);
    }

    #[test]
    fn test_launcher_exit_default() {
        let e = LauncherExit::default();
        assert!(e.exit_code.is_none());
        assert!(e.stdout.is_none());
        assert!(e.stderr.is_none());
    }

    /// The wall timeout Duration derives from the ms field, not secs.
    #[test]
    fn test_fence_wall_timeout_millis() {
        let f = FenceConfig {
            wall_timeout_ms: 500,
            ..FenceConfig::default()
        };
        let d = std::time::Duration::from_millis(f.wall_timeout_ms);
        assert_eq!(d.as_millis(), 500);
    }
}
