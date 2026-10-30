//! macOS Seatbelt sandbox session. Commands run via sandbox-exec -p PROFILE
//! -- SHELL -c CMD with the workspace as cwd, where SHELL is bash unless the
//! host has none. The shell is non-login so it sources no profile; PATH comes
//! from the parent process and the env snapshot. The profile denies default, allows system reads + the
//! workspace read/write + system-binary exec, and denies the network unless the
//! session carries a policy that opens it. Drop
//! removes the workspace only when this session created it (tempdir); a
//! user-cwd workspace is left untouched.
//!
//! nix is a safe FFI wrapper, so this module has zero unsafe blocks. The nix
//! dependency is target-gated to unix in the manifest; this module is only
//! compiled on macOS.

use crate::{ProfileSpec, ShellSnapshot, render};
use houyicoder_api::capture::RawOutputReceiver;
use houyicoder_api::sandbox::{
    Containment, Coverage, FenceCapability, FenceStatus, NetworkPolicy, SandboxSession, SideEffect,
    WorktreeFenceGuard, normalize_tool_path,
};
use houyicoder_async::PFut;
use houyicoder_context::{ExecConfig, ExecResult, SandboxError};
use houyicoder_resilience::resource_breaker::{ResourceBreaker, SpawnEvent};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

mod command;
pub use command::command_shell;
mod process;
mod temp_dir;
mod worktree;
use crate::output_capture::{CapturedOutput, DRAIN_GRACE, capture, pipe_streams};
use crate::runtime_dirs::RuntimeDirs;
use crate::shell_selection::ShellProgram;
use crate::timeout;
use command::annotate_fence_refusal;
use process::{BreakerSpanGuard, ExecCountGuard, TreeKillGuard, tree_cpu_secs};
use std::env::var;
use std::fs::remove_dir_all;
use std::process::id;
use std::sync::atomic::AtomicI64;
use std::time::Duration;
use temp_dir::mkdtemp;
use tokio::process::Command;
use tokio::time::timeout;

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
        let snapshot_shell = command::login_shell().unwrap_or_else(|| shell.clone());
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
        let snapshot_shell = command::login_shell().unwrap_or_else(|| shell.clone());
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

    /// The seatbelt profile string for the next exec: the pre-rendered
    /// empty-additional profile when no dirs are added AND no narrow is
    /// active, or a fresh render that includes the runtime-added dirs in the
    /// allow-back. When narrowed, the profile binds the WORKTREE (not the
    /// original workspace) + appends the .git/config read-allow (git needs
    /// repo config; the write deny still holds). Split out of exec so the
    /// re-derivation is unit-testable without spawning sandbox-exec. Each
    /// sandbox-exec is a fresh process, so mutating the profile between execs
    /// honors the new dirs (a running sandbox cannot be relaxed, but none is
    /// running between execs).
    fn current_profile(&self) -> String {
        let read_only = self.dirs.read_only();
        let read_write = self.dirs.read_write();
        let mach = self
            .extra_mach_services
            .lock()
            .expect("extra mach services lock")
            .clone();
        let allow_app_launch = *self.allow_app_launch.lock().expect("allow app launch lock");
        let narrow_ws = self
            .narrow_workspace
            .lock()
            .expect("narrow workspace lock")
            .clone();
        let narrow_git = self
            .narrow_git_common
            .lock()
            .expect("narrow git common lock")
            .clone();
        if read_only.is_empty()
            && read_write.is_empty()
            && narrow_ws.is_none()
            && mach.is_empty()
            && !allow_app_launch
        {
            return self.profile.clone();
        }
        let home = var("HOME").unwrap_or_else(|_| "/Users/unknown".into());
        let read_refs: Vec<&str> = read_only
            .iter()
            .map(|path| path.to_str().unwrap_or(""))
            .collect();
        let write_refs: Vec<&str> = read_write
            .iter()
            .map(|path| path.to_str().unwrap_or(""))
            .collect();
        let mach_refs: Vec<&str> = mach.iter().map(String::as_str).collect();
        // When narrowed, the profile binds the worktree; else the workspace.
        let ws = narrow_ws.as_ref().unwrap_or(&self.workspace);
        let mut p = render(
            &ProfileSpec::new(ws, &self.tmpdir.to_string_lossy(), &home, &self.tag)
                .with_additional(&write_refs)
                .with_read_only(&read_refs)
                .with_mach_services(&mach_refs)
                .with_app_launch(allow_app_launch)
                .with_network(self.network.clone()),
        );
        if let Some(git_common) = narrow_git.as_ref() {
            // Amendment: read-allow .git/config (git needs repo config) +
            // the worktree metadata dir. Appended AFTER mandatory_deny so
            // last-match-wins re-allows the read; the write deny still holds
            // (blocks credential-helper insertion; the network fence blocks
            // exfil even if config pointed at a helper).
            p.push_str(&format!(
                "(allow file-read* (literal \"{}\") (subpath \"{}\"))\n",
                git_common.join("config").to_string_lossy(),
                git_common.join("worktrees").to_string_lossy(),
            ));
        }
        p
    }

    fn resolve_path(&self, path: &str, include_read_only: bool) -> Result<PathBuf, SandboxError> {
        let workspace = self.effective_workspace();
        let supplied = Path::new(path);
        let base = if supplied.is_absolute() {
            supplied.to_path_buf()
        } else {
            workspace.join(path)
        };
        let canonical = normalize_tool_path(&base).unwrap_or(base);
        if canonical.starts_with(&workspace) || canonical.starts_with(&self.tmpdir) {
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

    /// The current effective workspace root: the narrow worktree when
    /// narrowed, else the original workspace.
    fn effective_workspace(&self) -> PathBuf {
        self.narrow_workspace
            .lock()
            .expect("narrow workspace lock")
            .clone()
            .unwrap_or_else(|| self.workspace.clone())
    }
}

impl Default for MacSeatbeltSession {
    fn default() -> Self {
        Self::new().expect("mac seatbelt session")
    }
}

impl Containment for MacSeatbeltSession {
    fn coverage(&self) -> Coverage {
        let root = self
            .narrow_workspace
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .unwrap_or_else(|| self.workspace.clone());
        Coverage::Fenced {
            writable_roots: vec![root],
        }
    }

    fn would_block(&self, effect: SideEffect) -> Option<String> {
        match effect {
            SideEffect::Network if !self.network.allows_egress() => {
                Some("egress is contained".into())
            }
            _ => None,
        }
    }

    /// The seatbelt profile carries file rules for both filesystem axes.
    /// The network axis follows the session policy: a contained policy emits
    /// the deny, so the kernel decides egress; an open policy leaves every
    /// destination reachable and therefore contains nothing on that axis.
    fn capability(&self) -> FenceCapability {
        FenceCapability {
            writes: true,
            reads: true,
            network: !self.network.allows_egress(),
        }
    }

    fn capability_revision(&self) -> u64 {
        self.revision.load(Ordering::SeqCst)
    }

    /// The profile carries explicit write denies for the settings and
    /// permission stores, the credential files, and the skill and hook
    /// directories, emitted after every allow, so the kernel refuses those
    /// writes even inside a workspace it opened.
    fn denies_control_writes(&self) -> bool {
        true
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

impl MacSeatbeltSession {
    /// Record that the fence now contains a different set of paths. Only a
    /// grant that actually changed the set advances the counter, so a
    /// repeated install does not look like a fresh capability.
    fn note_capability_change(&self) {
        self.revision.fetch_add(1, Ordering::SeqCst);
    }

    #[expect(clippy::disallowed_methods, reason = "infra spawn, not model-driven")]
    async fn exec_inner(
        &self,
        command: String,
        config: ExecConfig,
        lines: Option<Arc<AtomicI64>>,
        receiver: Option<Arc<dyn RawOutputReceiver>>,
    ) -> Result<ExecResult, SandboxError> {
        let profile = self.current_profile();
        let cwd = self.effective_workspace();
        // Wrap the command to source the login-shell env snapshot first, so
        // the non-login shell still sees the user's PATH / aliases /
        // functions. No-op when the snapshot is unavailable (degraded). The
        // snapshot file lives under the config home, which the profile
        // allow-backs so the sandboxed shell can read it.
        let command = self
            .shell_snapshot
            .lock()
            .expect("shell_snapshot mutex")
            .inject(&command, Some(&self.tmpdir.to_string_lossy()));
        let breaker = self.breaker.clone();
        // A worktree session used to run with HOME rewritten to the worktree
        // plus the system git config switched off, so git would not touch two
        // paths the profile denied. Both denials are gone: the etc allow-back
        // now reaches the kernel, and the home git config is readable with the
        // write still denied. The environment is therefore left alone, which
        // matters because rewriting HOME also hid the user's real git identity
        // and aliases from every command in the session.
        let exec_count = Arc::clone(&self.exec_count);
        exec_count.fetch_add(1, Ordering::SeqCst);
        // RAII guard: decrement the in-flight exec count on EVERY exit
        // path (Ok, Err, wall-timeout, future-drop/cancel) so a worktree
        // enter's active_exec_count check is not fooled by a leaked count.
        let _exec_guard = ExecCountGuard(Arc::clone(&exec_count));
        // Aggregate breaker: refuse the spawn up front while Open (the
        // cool-down after an aggregate overrun). Fail-closed toward
        // refusal — a breaker that has tripped must not silently let a
        // new spawn through. No pgid/guard yet; we never spawned.
        if let Some(b) = &breaker
            && let Err(e) = b.try_acquire()
        {
            return Err(SandboxError::BreakerOpen(e.to_string()));
        }
        // A session may run unfenced (via the --no-sandbox flag or the
        // HOUYICODER_SANDBOX_NO_ENFORCE test escape hatch). The kernel refuses
        // to apply a second seatbelt inside an existing one, so a suite running
        // under a session's own fence cannot spawn sandbox-exec at all.
        let unfenced = crate::is_unfenced();
        let mut cmd = if unfenced {
            Command::new(&self.shell)
        } else {
            Command::new("sandbox-exec")
        };
        // Export the per-session temp dir as TMPDIR so heredoc temp files and
        // tools that honor TMPDIR land inside the fence (the profile
        // allow-lists this exact path). TMPPREFIX covers a host that fell back
        // to zsh, whose heredoc temp ignores TMPDIR; bash ignores it. Set
        // explicitly rather than inheriting the parent TMPDIR, which points
        // outside the fence.
        let tmpdir_str = self.tmpdir.to_string_lossy().into_owned();
        cmd.env("TMPDIR", &tmpdir_str)
            .env("TMPPREFIX", format!("{tmpdir_str}/zsh"));
        if !unfenced {
            // The profile and its separator belong to the fenced form only.
            // The program under it is sandbox-exec, so the shell is an
            // argument. Unfenced there is no wrapper, and the shell IS the
            // program, so passing it again would ask bash to execute itself.
            cmd.arg("-p").arg(&profile).arg("--").arg(&self.shell);
        }
        cmd.arg("-c").arg(&command).current_dir(&cwd);
        pipe_streams(&mut cmd);
        cmd.kill_on_drop(true);
        // process_group(0) puts child + all descendants in a NEW process
        // group (pgid = child pid) so killpg(pgid) reaps the whole tree on
        // timeout — the direct-child-only kill leaves orphan
        // fix. tokio's process_group is a safe wrapper (no pre_exec, which
        // std marks unsafe and the workspace denies).
        cmd.process_group(0);
        // The cpu, address space and process count budgets are not applied
        // on macOS: there is no safe in-child setrlimit (pre_exec is
        // unsafe-blocked), so the resource fence here is the wall timeout
        // plus the group kill above. Linux defers them for a different
        // reason: the rlimit primitives are per user, not per tree.
        let _ = (config.cpu_secs, config.as_bytes, config.nproc);
        let child = cmd
            .spawn()
            .map_err(|e| SandboxError::SandboxUnavailable(format!("sandbox-exec spawn: {e}")))?;
        // After process_group(0) the child's pgid equals its pid.
        let pgid = child.id().unwrap_or(0) as i32;
        // RAII tree-kill guard: on Drop (every exit path — Ok, Err,
        // wall-timeout, AND future-drop/Ctrl-C) killpg the whole group.
        // Without this, dropping the exec future (cancel) would only
        // kill_on_drop the direct child, leaving grandchildren alive —
        // re-introducing the orphan-process problem.
        let _tree_guard = TreeKillGuard::new(pgid);
        // Count one in-flight proc + arm an RAII guard that records the
        // matching End on EVERY exit path, including future-drop/cancel
        // (Ctrl-C). Without it, a cancel between Start and the End call
        // would leak the in-flight count and skip the burned-CPU
        // accounting, false-tripping the breaker after enough cancels.
        let mut breaker_guard = if let Some(b) = &breaker {
            b.record(SpawnEvent::Start { proc_count: 1 });
            Some(BreakerSpanGuard::new(b.clone(), pgid))
        } else {
            None
        };
        let wall = Duration::from_millis(config.wall_timeout_ms);
        // Both entry points share one drain, so what a command printed is
        // kept on a stop either way. The streaming entry point passes a
        // counter for the live line count; the plain one passes none. The
        // wall-timeout wraps the drain; on timeout tokio cancels it → child
        // drops (kill_on_drop) → _tree_guard killpg, and the bytes already
        // read stay in collected for the caller.
        let collected = CapturedOutput::default();
        let outcome = timeout(
            wall,
            capture(
                child,
                lines.as_deref(),
                &collected,
                DRAIN_GRACE,
                receiver.as_deref(),
            ),
        )
        .await;
        // Measure the tree's CPU while _tree_guard still holds the group
        // alive. Drop order is reverse declaration: breaker_guard drops
        // BEFORE _tree_guard, so on the cancel path the tree is still
        // live when the guard measures. On a wall-timeout the spinning
        // descendants are still visible to ps; on a clean exit they were
        // already reaped and this reads ~0.
        let cpu_secs = breaker.as_ref().map(|_| tree_cpu_secs(pgid)).unwrap_or(0);
        let mut result = match outcome {
            Ok(Ok(output)) => Ok(ExecResult {
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                exit_code: output.status.code(),
            }),
            Ok(Err(e)) => Err(SandboxError::Io(format!("sandbox-exec drain: {e}"))),
            Err(_elapsed) => Err(collected.timeout_error(timeout::message(config.wall_timeout_ms))),
        };
        if let Ok(exec) = &mut result {
            // The narrowed root is the one the profile allows; the original
            // workspace is refused in a worktree session, so naming it would
            // send the reader to a second denial.
            annotate_fence_refusal(exec, &self.effective_workspace(), &self.tmpdir);
        }
        // Feed the measured outcome to the guard; its Drop records End on
        // this path AND on the cancel path (where it measures CPU itself).
        // A user cancel is not a budget exceed (only Timeout /
        // ResourceLimitExceeded are) — it does not bump the consecutive
        // counter, though its burned CPU still lands in the aggregate.
        if let Some(g) = breaker_guard.as_mut() {
            let exceeded_budget = matches!(
                &result,
                Err(SandboxError::Timeout { .. } | SandboxError::ResourceLimitExceeded(_))
            );
            g.finish(cpu_secs, exceeded_budget);
        }
        result
        // Locals drop in reverse order: breaker_guard (records End) then
        // _tree_guard (killpg). On cancel, breaker_guard measures CPU
        // while the tree is still live, then _tree_guard reaps it.
    }
}

impl SandboxSession for MacSeatbeltSession {
    /// Enforced whenever the session exists: every command goes through
    /// sandbox-exec with the rendered profile.
    fn fence_status(&self) -> FenceStatus {
        FenceStatus::Enforced
    }

    fn as_containment(&self) -> Option<&dyn Containment> {
        Some(self)
    }
    /// The child is spawned via sandbox-exec (the seatbelt spawn path), not
    /// std::process::Command directly, so disallowed_methods does not fire.
    fn exec_with_config(
        &self,
        command: &str,
        config: ExecConfig,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        let command = command.to_string();
        Box::pin(async move { self.exec_inner(command, config, None, None).await })
    }

    /// Streaming variant: the drain updates the shared line counter as
    /// newlines arrive, so the host's bash chip can show "(12s · 14 lines)"
    /// on a long-running command. Same fence + timeout + tree-kill
    /// invariants as exec_with_config; only the counter differs.
    fn exec_streaming(
        &self,
        command: &str,
        lines: Arc<AtomicI64>,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        self.exec_streaming_with_config(command, lines, ExecConfig::default())
    }

    fn exec_streaming_with_config(
        &self,
        command: &str,
        lines: Arc<AtomicI64>,
        config: ExecConfig,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        let command = command.to_string();
        Box::pin(async move { self.exec_inner(command, config, Some(lines), None).await })
    }

    fn exec_streaming_captured(
        &self,
        command: &str,
        lines: Arc<AtomicI64>,
        config: ExecConfig,
        receiver: Option<Arc<dyn RawOutputReceiver>>,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        let command = command.to_string();
        Box::pin(async move {
            self.exec_inner(command, config, Some(lines), receiver)
                .await
        })
    }

    fn resolve(&self, path: &str) -> Result<PathBuf, SandboxError> {
        self.resolve_path(path, true)
    }

    fn resolve_write(&self, path: &str) -> Result<PathBuf, SandboxError> {
        self.resolve_path(path, false)
    }

    fn workspace_root(&self) -> Arc<Path> {
        Arc::from(self.effective_workspace())
    }

    fn scratch_dir(&self) -> Option<PathBuf> {
        Some(self.tmpdir.clone())
    }

    fn active_exec_count(&self) -> usize {
        self.exec_count.load(Ordering::SeqCst) as usize
    }

    fn narrow_to_worktree(
        &self,
        worktree: &Path,
        git_common_dir: &Path,
    ) -> Result<WorktreeFenceGuard, SandboxError> {
        worktree::narrow_to_worktree(self, worktree, git_common_dir)
    }

    fn add_working_dir(&self, path: &str) -> Result<(), SandboxError> {
        if self.dirs.add_write(path)? {
            self.note_capability_change();
        }
        Ok(())
    }

    fn add_reading_dir(&self, path: &str) -> Result<(), SandboxError> {
        if self.dirs.add_read(path)? {
            self.note_capability_change();
        }
        Ok(())
    }

    fn remove_working_dir(&self, path: &str) {
        if self.dirs.remove(path) {
            self.note_capability_change();
        }
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

    fn set_extra_mach_services(&self, services: &[String]) {
        let mut mach = self
            .extra_mach_services
            .lock()
            .expect("extra mach services lock");
        mach.clear();
        for service in services {
            if !mach.contains(service) {
                mach.push(service.clone());
            }
        }
    }

    fn clear_extra_mach_services(&self) {
        self.extra_mach_services
            .lock()
            .expect("extra mach services lock")
            .clear();
    }

    fn grant_app_launch(&self) {
        *self.allow_app_launch.lock().expect("allow app launch lock") = true;
    }

    fn clear_skill_grants(&self) {
        self.extra_mach_services
            .lock()
            .expect("extra mach services lock")
            .clear();
        *self.allow_app_launch.lock().expect("allow app launch lock") = false;
    }

    // Window-scoped, not pid-scoped: the deny log records the command
    // process pid (a grandchild of sandbox-exec), which the session does
    // not hold. A short window (5s) bounds the cross-process noise risk.
    fn discover_authorizable(&self) -> Vec<String> {
        crate::deny_log::discover_authorizable(5)
    }
}

#[cfg(test)]
mod tests;
