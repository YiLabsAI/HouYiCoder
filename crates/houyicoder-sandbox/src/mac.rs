//! macOS Seatbelt sandbox session. Commands run via sandbox-exec -p PROFILE
//! -- SHELL -c CMD with the workspace as cwd, where SHELL is bash unless the
//! host has none; the shell is non-login, so PATH comes from the parent and
//! the env snapshot. The profile denies by default, allows system reads,
//! workspace read/write, and system-binary exec, and opens the network only
//! under a session policy. nix is a safe FFI wrapper, so this module has zero
//! unsafe blocks; its dependency is target-gated to unix and only this module
//! compiles on macOS.

use std::env::var;
use std::fs::remove_dir_all;
use std::path::{Path, PathBuf};
use std::process::id;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;

use houyicoder_api::sandbox::NetworkPolicy;
use houyicoder_context::SandboxError;
use houyicoder_resilience::resource_breaker::ResourceBreaker;

use crate::runtime_dirs::RuntimeDirs;
use crate::shell_selection::ShellProgram;
use crate::{ProfileSpec, ShellSnapshot, render};

mod command;
pub use command::command_shell;
mod exec;
mod fence;
mod process;
mod profile;
mod temp_dir;
mod worktree;

use command::login_shell;
use temp_dir::mkdtemp;

/// A macOS Seatbelt sandbox session. The workspace is a temp dir, the user's
/// project dir, or a linked git worktree owned by this session (the Isolated
/// mode). Drop removes the workspace only when this session created it.
pub struct MacSeatbeltSession {
    workspace: PathBuf,
    /// Per-session temp directory this session creates under the system temp
    /// root and owns (Drop removes it). The seatbelt profile allow-lists it
    /// for reads and writes, resolve() admits paths under it so the Write and
    /// Edit tools can write temp files, and exec exports TMPDIR (and
    /// TMPPREFIX for zsh) pointing at it so heredoc temp files and tools that
    /// honor TMPDIR land inside the fence. Without it the agent is forced to
    /// inline minus-c scripts or pollute the workspace root, and heredocs fail
    /// because their temp files land outside the fence.
    tmpdir: PathBuf,
    profile: String,
    /// The seatbelt tag (pid-derived) stored so exec can re-render the
    /// profile with the runtime-added working dirs without recomputing it.
    tag: String,
    /// Runtime directory grants, separated by write capability.
    dirs: RuntimeDirs,
    /// Extra mach services (XPC names) a skill declared, consulted by
    /// current_profile to emit allow mach-lookup lines beyond the base set.
    /// Set by set_extra_mach_services, cleared by clear_extra_mach_services.
    /// Same Arc-Mutex pattern as additional_dirs so a guard closure can clear
    /// through a clone.
    extra_mach_services: Arc<Mutex<Vec<String>>>,
    /// Whether the LaunchServices app-launch entitlement is granted so a
    /// sandboxed process can open -a an app. Set by grant_app_launch.
    allow_app_launch: Arc<Mutex<bool>>,
    /// When a worktree session narrows the fence, this holds the worktree path
    /// that current_profile + exec use as the workspace + cwd (None = the
    /// original workspace, the default guarded mode). Set by narrow_to_worktree,
    /// cleared by the guard restore. Arc-shared so the restore closure (in the
    /// api-crate guard) can revert through a clone.
    narrow_workspace: Arc<Mutex<Option<PathBuf>>>,
    /// The repo .git common dir the worktree session allow-backed, so
    /// current_profile can append the .git/config read-allow (git needs to
    /// read repo config; the write stays denied). None unless narrowed.
    /// Arc-shared for the restore closure.
    narrow_git_common: Arc<Mutex<Option<PathBuf>>>,
    /// Count of in-flight exec spawns, so a worktree enter can refuse while a
    /// long-running spawn (which keeps its wide-fence profile alive) is still
    /// running. Best-effort: a cancel between Start and the matching End leaks
    /// one count (the breaker's own tracking is the reliable backstop).
    exec_count: Arc<AtomicU64>,
    /// True only for the tempdir workspace this session created (Drop removes
    /// it). False for the user cwd (never delete the user's directory).
    owned: bool,
    /// Aggregate resource breaker shared across all exec calls in this
    /// session. None means no aggregate breaker — each spawn is still
    /// individually fenced by the per-cmd wall-timeout + tree-kill. When set,
    /// exec consults try_acquire before spawn and records the command's CPU +
    /// budget outcome so the breaker trips Open on aggregate overrun, then
    /// refuses new spawns for the cool-down.
    breaker: Option<Arc<ResourceBreaker>>,
    /// How wide the network fence is opened. Held so that the per-exec profile
    /// re-render (which happens whenever a dir was added or the fence narrowed)
    /// carries the same posture as the profile rendered at construction. Storing
    /// it here rather than re-reading settings per exec keeps one posture for the
    /// life of the session, so a settings edit mid-session cannot silently widen
    /// the fence under a running agent.
    network: NetworkPolicy,
    /// Advances whenever the effective capability changes: a directory
    /// grant is added or revoked, or the fence narrows to a worktree and is
    /// restored. Shared so the worktree restore closure advances it through
    /// a clone. A caller that installs a grant reads it before and after, so
    /// an install the fence did not apply is caught rather than reported as
    /// saved.
    revision: Arc<AtomicU64>,
    /// Login-shell env snapshot: captures PATH/aliases/functions once per
    /// session by running the user's login shell, then sources the snapshot
    /// into each sandboxed command so the non-login shell still sees the
    /// user's environment (homebrew PATH, rc aliases).
    shell_snapshot: Mutex<ShellSnapshot>,
    /// The shell a sandboxed command runs under: bash when the host has one,
    /// the login shell otherwise. The snapshot holds plain exported variables,
    /// so either shell can source it.
    shell: PathBuf,
}

impl MacSeatbeltSession {
    /// Create a new session: mint a temp workspace dir and render the Seatbelt
    /// profile bound to it. Used by tests and as a fallback when no cwd is
    /// available. The tempdir is removed on Drop.
    pub fn new() -> Result<Self, SandboxError> {
        let workspace = mkdtemp("hy-work")?;
        // Canonicalize so mac /var -> /private/var symlinks do not make
        // starts_with checks in resolve() reject legitimate workspace paths.
        let workspace = dunce::canonicalize(&workspace)?;
        let tmpdir = mkdtemp("hy-tmp")?;
        let tmpdir = dunce::canonicalize(&tmpdir)?;
        let home = var("HOME").unwrap_or_else(|_| "/Users/unknown".into());
        let shell = command_shell();
        let snapshot_shell = login_shell().unwrap_or_else(|| shell.clone());
        let tag = format!("houyicoder-sandbox-{}", id());
        let profile = render(&ProfileSpec::new(
            &workspace,
            &tmpdir.to_string_lossy(),
            &home,
            &tag,
        ));
        Ok(Self {
            workspace,
            tmpdir,
            profile,
            tag,
            dirs: RuntimeDirs::default(),
            extra_mach_services: Arc::new(Mutex::new(Vec::new())),
            allow_app_launch: Arc::new(Mutex::new(false)),
            narrow_workspace: Arc::new(Mutex::new(None)),
            narrow_git_common: Arc::new(Mutex::new(None)),
            exec_count: Arc::new(AtomicU64::new(0)),
            owned: true,
            breaker: None,
            network: NetworkPolicy::contained(),
            revision: Arc::new(AtomicU64::new(0)),
            shell_snapshot: Mutex::new(ShellSnapshot::new(snapshot_shell, PathBuf::from(&home))),
            shell,
        })
    }

    /// Guarded mode (default): the workspace IS the user's project directory.
    /// The agent edits the real tree directly (changes immediate), and the
    /// seatbelt fences it: no writes outside the project, and no network unless
    /// a policy opens it. The
    /// user's directory is never removed on Drop.
    pub fn new_in_cwd(cwd: &Path) -> Result<Self, SandboxError> {
        let workspace = dunce::canonicalize(cwd)
            .map_err(|e| SandboxError::Io(format!("cwd canonicalize: {e}")))?;
        let tmpdir = mkdtemp("hy-tmp")?;
        let tmpdir = dunce::canonicalize(&tmpdir)?;
        let home = var("HOME").unwrap_or_else(|_| "/Users/unknown".into());
        let shell = command_shell();
        let snapshot_shell = login_shell().unwrap_or_else(|| shell.clone());
        let tag = format!("houyicoder-sandbox-{}", id());
        let profile = render(&ProfileSpec::new(
            &workspace,
            &tmpdir.to_string_lossy(),
            &home,
            &tag,
        ));
        Ok(Self {
            workspace,
            tmpdir,
            profile,
            tag,
            dirs: RuntimeDirs::default(),
            extra_mach_services: Arc::new(Mutex::new(Vec::new())),
            allow_app_launch: Arc::new(Mutex::new(false)),
            narrow_workspace: Arc::new(Mutex::new(None)),
            narrow_git_common: Arc::new(Mutex::new(None)),
            exec_count: Arc::new(AtomicU64::new(0)),
            owned: false,
            breaker: None,
            network: NetworkPolicy::contained(),
            revision: Arc::new(AtomicU64::new(0)),
            shell_snapshot: Mutex::new(ShellSnapshot::new(snapshot_shell, PathBuf::from(&home))),
            shell,
        })
    }

    /// Attach a shared aggregate resource breaker. The breaker is consulted by
    /// every exec call: a new spawn is refused while Open, and each command's
    /// CPU + budget outcome is recorded so aggregate overrun trips the breaker
    /// across the whole run. The session holds the Arc so every tool sharing
    /// this session aggregates against one breaker. The composition root (the
    /// runner wiring) constructs the breaker and attaches it here, keeping the
    /// SandboxSession trait unchanged.
    pub fn with_breaker(mut self, breaker: Arc<ResourceBreaker>) -> Self {
        self.breaker = Some(breaker);
        self
    }

    /// Set the network posture and re-render the cached profile so the stored
    /// profile and the policy cannot disagree. Re-rendering here rather than
    /// storing the policy alone is deliberate: the cached profile is what exec
    /// uses on the common path, so leaving it stale would make the setter appear
    /// to work while the fence stayed contained.
    #[must_use]
    pub fn with_network(mut self, network: NetworkPolicy) -> Self {
        let home = var("HOME").unwrap_or_else(|_| "/Users/unknown".into());
        self.profile = render(
            &ProfileSpec::new(
                &self.workspace,
                &self.tmpdir.to_string_lossy(),
                &home,
                &self.tag,
            )
            .with_network(network.clone()),
        );
        self.network = network;
        self
    }

    /// No-op: this host runs its own shell, so there is no program to select.
    /// Present for PlatformSession parity; a settings value naming a Windows
    /// shell is reported by the selection before it reaches this session.
    #[must_use]
    pub fn with_shell_program(self, _program: Option<ShellProgram>) -> Self {
        self
    }
}

impl Default for MacSeatbeltSession {
    fn default() -> Self {
        Self::new().expect("mac seatbelt session")
    }
}

impl Drop for MacSeatbeltSession {
    fn drop(&mut self) {
        // Only a tempdir workspace this session created is removed here; a
        // user-cwd workspace is never deleted. Best-effort; never panic.
        if self.owned {
            let _result = remove_dir_all(&self.workspace);
        }
        // The per-session temp dir is always created by this session, so it is
        // always removed here (unlike the workspace, which may be a user cwd).
        // Best-effort; never panic.
        let _result = remove_dir_all(&self.tmpdir);
    }
}

#[cfg(test)]
mod tests;
