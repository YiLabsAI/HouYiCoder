//! macOS Seatbelt sandbox session. Commands run via sandbox-exec -p PROFILE
//! -- /bin/sh -c CMD with the workspace as cwd. The shell is non-login so it
//! sources no profile (no /etc/profile, no ~/.profile) — PATH is inherited from
//! the parent process. The profile denies default, allows system reads + the
//! workspace read/write + system-binary exec, and denies the network unless the
//! session carries a policy that opens it. Drop
//! removes the workspace only when this session created it (tempdir); a
//! user-cwd workspace is left untouched.
//!
//! nix is a safe FFI wrapper, so this module has zero unsafe blocks. The nix
//! dependency is target-gated to unix in the manifest; this module is only
//! compiled on macOS.

use crate::{ProfileSpec, ShellSnapshot, render};
use houyicoder_api::sandbox::{
    Containment, Coverage, FenceStatus, NetworkPolicy, SandboxSession, SideEffect,
    WorktreeFenceGuard,
};
use houyicoder_async::PFut;
use houyicoder_context::{ExecConfig, ExecResult, SandboxError};
use houyicoder_resilience::resource_breaker::{ResourceBreaker, SpawnEvent};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

mod helpers;
mod seatbelt_stream;
#[cfg(test)]
use helpers::COUNTER;
use helpers::{ExecCountGuard, kill_process_group, mkdtemp, tree_cpu_secs};
use seatbelt_stream::stream_drain;

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
    /// Directories the user added to the workspace at runtime, consulted by
    /// exec to extend the seatbelt allow-back. Canonicalized + deduped under
    /// the lock; the kernel fence is re-derived per exec so a dir added
    /// mid-session takes effect on the next command (seatbelt profiles are
    /// monotonic per-process, so the fence cannot be relaxed inside a running
    /// sandbox — but each sandbox-exec is a fresh process with the current
    /// profile string, so mutation between execs is honored).
    additional_dirs: Arc<Mutex<Vec<PathBuf>>>,
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
    /// Login-shell env snapshot: captures PATH/aliases/functions once per
    /// session by running the user's login shell, then sources the snapshot
    /// into each sandboxed command so the non-login sh shell still sees the
    /// user's environment (homebrew PATH, rc aliases).
    shell_snapshot: Mutex<ShellSnapshot>,
    /// The user's login shell ($SHELL), used to run sandboxed commands so they
    /// match the env snapshot (a zsh snapshot sources cleanly in zsh; the old
    /// /bin/sh mismatched the snapshot's shell and aborted on zsh syntax).
    shell: PathBuf,
}

impl MacSeatbeltSession {
    /// Create a new session: mint a temp workspace dir and render the Seatbelt
    /// profile bound to it. Used by tests and as a fallback when no cwd is
    /// available. The tempdir is removed on Drop.
    pub fn new() -> Result<Self, SandboxError> {
        let workspace = mkdtemp("houyicoder-sandbox")?;
        // Canonicalize so mac /var -> /private/var symlinks do not make
        // starts_with checks in resolve() reject legitimate workspace paths.
        let workspace = dunce::canonicalize(&workspace)?;
        let tmpdir = mkdtemp("houyicoder-tmp")?;
        let tmpdir = dunce::canonicalize(&tmpdir)?;
        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/unknown".into());
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let tag = format!("houyicoder-sandbox-{}", std::process::id());
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
            additional_dirs: Arc::new(Mutex::new(Vec::new())),
            narrow_workspace: Arc::new(Mutex::new(None)),
            narrow_git_common: Arc::new(Mutex::new(None)),
            exec_count: Arc::new(AtomicU64::new(0)),
            owned: true,
            breaker: None,
            network: NetworkPolicy::contained(),
            shell_snapshot: Mutex::new(ShellSnapshot::new(
                PathBuf::from(&shell),
                PathBuf::from(&home),
            )),
            shell: PathBuf::from(&shell),
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
        let tmpdir = mkdtemp("houyicoder-tmp")?;
        let tmpdir = dunce::canonicalize(&tmpdir)?;
        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/unknown".into());
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let tag = format!("houyicoder-sandbox-{}", std::process::id());
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
            additional_dirs: Arc::new(Mutex::new(Vec::new())),
            narrow_workspace: Arc::new(Mutex::new(None)),
            narrow_git_common: Arc::new(Mutex::new(None)),
            exec_count: Arc::new(AtomicU64::new(0)),
            owned: false,
            breaker: None,
            network: NetworkPolicy::contained(),
            shell_snapshot: Mutex::new(ShellSnapshot::new(
                PathBuf::from(&shell),
                PathBuf::from(&home),
            )),
            shell: PathBuf::from(&shell),
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
        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/unknown".into());
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
        let additional = self.additional_dirs.lock().expect("additional dirs lock");
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
        if additional.is_empty() && narrow_ws.is_none() {
            return self.profile.clone();
        }
        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/unknown".into());
        let add_refs: Vec<&str> = additional
            .iter()
            .map(|p| p.to_str().unwrap_or(""))
            .collect();
        // When narrowed, the profile binds the worktree; else the workspace.
        let ws = narrow_ws.as_ref().unwrap_or(&self.workspace);
        let mut p = render(
            &ProfileSpec::new(ws, &self.tmpdir.to_string_lossy(), &home, &self.tag)
                .with_additional(&add_refs)
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
}

impl Drop for MacSeatbeltSession {
    fn drop(&mut self) {
        // Only a tempdir workspace this session created is removed here; a
        // user-cwd workspace is never deleted. Best-effort; never panic.
        if self.owned {
            let _result = std::fs::remove_dir_all(&self.workspace);
        }
        // The per-session temp dir is always created by this session, so it is
        // always removed here (unlike the workspace, which may be a user cwd).
        // Best-effort; never panic.
        let _result = std::fs::remove_dir_all(&self.tmpdir);
    }
}

impl MacSeatbeltSession {
    #[expect(clippy::disallowed_methods, reason = "infra spawn, not model-driven")]
    async fn exec_inner(
        &self,
        command: String,
        config: ExecConfig,
        lines: Option<std::sync::Arc<std::sync::atomic::AtomicI64>>,
    ) -> Result<ExecResult, SandboxError> {
        let profile = self.current_profile();
        let cwd = self.effective_workspace();
        // Wrap the command to source the login-shell env snapshot first, so
        // the non-login sh shell still sees the user's PATH / aliases /
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
        let mut cmd = tokio::process::Command::new("sandbox-exec");
        // Export the per-session temp dir as TMPDIR so heredoc temp files
        // and tools that honor TMPDIR land inside the fence (the profile
        // allow-lists this exact path). TMPPREFIX routes zsh's heredoc
        // temp there too (zsh ignores TMPDIR for heredoc temp files);
        // non-zsh shells ignore TMPPREFIX. Set explicitly rather than
        // inheriting the parent TMPDIR, which points outside the fence.
        let tmpdir_str = self.tmpdir.to_string_lossy().into_owned();
        cmd.env("TMPDIR", &tmpdir_str)
            .env("TMPPREFIX", format!("{tmpdir_str}/zsh"))
            .arg("-p")
            .arg(&profile)
            .arg("--")
            .arg(&self.shell)
            .arg("-c")
            .arg(&command)
            .current_dir(&cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        // process_group(0) puts child + all descendants in a NEW process
        // group (pgid = child pid) so killpg(pgid) reaps the whole tree on
        // timeout — the direct-child-only kill leaves orphan
        // fix. tokio's process_group is a safe wrapper (no pre_exec, which
        // std marks unsafe and the workspace denies).
        cmd.process_group(0);
        // cpu_secs/as_bytes/nproc are applied via Linux cgroup v2,
        // cpu.max/memory.max/pids.max, a safe config — no pre_exec). macOS
        // has no safe in-child setrlimit (pre_exec is unsafe-blocked), so
        // the macOS fence is wall-timeout + killpg; cgroup does the per-cmd
        // CPU/memory budget on Linux where it is the stronger primitive.
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
        let wall = std::time::Duration::from_millis(config.wall_timeout_ms);
        // The ONLY branch that differs between exec_with_config and
        // exec_streaming: lines None → wait_with_output (the original path,
        // no behavior change); lines Some → stream_drain (concurrent stdout
        // + stderr drain + live line count). Both return io::Result<Output>,
        // so the match below is identical. The wall-timeout wraps the drain
        // the same way it wrapped wait_with_output; on timeout tokio cancels
        // the drain future → child drops (kill_on_drop) → _tree_guard killpg,
        // exactly the existing timeout path.
        let outcome = if let Some(lines) = lines.as_ref() {
            tokio::time::timeout(wall, stream_drain(child, lines)).await
        } else {
            tokio::time::timeout(wall, child.wait_with_output()).await
        };
        // Measure the tree's CPU while _tree_guard still holds the group
        // alive. Drop order is reverse declaration: breaker_guard drops
        // BEFORE _tree_guard, so on the cancel path the tree is still
        // live when the guard measures. On a wall-timeout the spinning
        // descendants are still visible to ps; on a clean exit they were
        // already reaped and this reads ~0.
        let cpu_secs = breaker.as_ref().map(|_| tree_cpu_secs(pgid)).unwrap_or(0);
        let result = match outcome {
            Ok(Ok(output)) => Ok(ExecResult {
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                exit_code: output.status.code(),
            }),
            Ok(Err(e)) => Err(SandboxError::Io(format!("sandbox-exec wait: {e}"))),
            Err(_elapsed) => Err(SandboxError::Timeout(format!(
                "wall-clock {}s exceeded",
                config.wall_timeout_ms
            ))),
        };
        // Feed the measured outcome to the guard; its Drop records End on
        // this path AND on the cancel path (where it measures CPU itself).
        // A user cancel is not a budget exceed (only Timeout /
        // ResourceLimitExceeded are) — it does not bump the consecutive
        // counter, though its burned CPU still lands in the aggregate.
        if let Some(g) = breaker_guard.as_mut() {
            let exceeded_budget = matches!(
                &result,
                Err(SandboxError::Timeout(_) | SandboxError::ResourceLimitExceeded(_))
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

    fn as_containment(&self) -> Option<&dyn houyicoder_api::sandbox::Containment> {
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
        Box::pin(async move { self.exec_inner(command, config, None).await })
    }

    /// Streaming variant: the backend drains stdout incrementally + updates
    /// the shared line counter as newlines arrive, so the host's bash chip
    /// can show "(12s · 14 lines)" on a long-running command. Same fence +
    /// timeout + tree-kill invariants as exec_with_config; only the stdout
    /// collection path differs (concurrent drain vs wait_with_output).
    fn exec_streaming(
        &self,
        command: &str,
        lines: std::sync::Arc<std::sync::atomic::AtomicI64>,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        let command = command.to_string();
        Box::pin(async move {
            self.exec_inner(command, ExecConfig::default(), Some(lines))
                .await
        })
    }

    /// Overrides the trait default: mac seatbelt authorizes external dirs
    /// (add_working_dir + the session tmpdir) that bash reaches via the
    /// re-derived profile, so absolute paths landing in an allowed root are
    /// admitted, not just workspace-relative ones.
    fn resolve(&self, path: &str) -> Result<PathBuf, SandboxError> {
        let ws = self.effective_workspace();
        let p = Path::new(path);
        let base = if p.is_absolute() {
            p.to_path_buf()
        } else {
            ws.join(path)
        };
        // Collapse .. by canonicalizing the parent when it exists; the kernel
        // fence is the real boundary, this is a supplementary application guard.
        let canonical = base
            .parent()
            .filter(|p| p.exists())
            .and_then(|p| dunce::canonicalize(p).ok())
            .map(|parent| parent.join(base.file_name().unwrap_or_default()))
            .unwrap_or(base.clone());
        if canonical.starts_with(&ws) {
            return Ok(canonical);
        }
        if canonical.starts_with(&self.tmpdir) {
            return Ok(canonical);
        }
        let dirs = self.additional_dirs.lock().expect("additional dirs lock");
        if dirs.iter().any(|d| canonical.starts_with(d)) {
            return Ok(canonical);
        }
        Err(SandboxError::PathTraversal(format!(
            "path escapes workspace + authorized dirs: {path}"
        )))
    }

    fn workspace_root(&self) -> Arc<Path> {
        Arc::from(self.effective_workspace())
    }

    fn active_exec_count(&self) -> usize {
        self.exec_count.load(Ordering::SeqCst) as usize
    }

    fn narrow_to_worktree(
        &self,
        worktree: &Path,
        git_common_dir: &Path,
    ) -> Result<WorktreeFenceGuard, SandboxError> {
        // Refuse while an in-flight exec keeps its wide-fence profile alive
        // (a backgrounded command could still write the main tree after the
        // narrow). The caller should wait or cancel first.
        if self.active_exec_count() > 0 {
            return Err(SandboxError::Unsupported(
                "worktree enter refused: a sandbox exec is in flight".into(),
            ));
        }
        let worktree = dunce::canonicalize(worktree)
            .map_err(|e| SandboxError::Io(format!("worktree canonicalize: {e}")))?;
        let git_common = dunce::canonicalize(git_common_dir)
            .map_err(|e| SandboxError::Io(format!("git common canonicalize: {e}")))?;
        // The worktree must sit inside the repo root (the original workspace)
        // — a path outside is an escape attempt.
        if !worktree.starts_with(&self.workspace) {
            return Err(SandboxError::PathTraversal(format!(
                "worktree path escapes the repo root: {}",
                worktree.display()
            )));
        }
        // Allow-back the repo .git so git ops can read/write objects + refs.
        self.add_working_dir(git_common.to_str().expect("git common path str"))?;
        // Record the narrow state: current_profile re-renders bound to the
        // worktree + appends the .git/config read-allow; exec uses the
        // worktree as cwd and leaves the environment untouched.
        *self.narrow_workspace.lock().expect("narrow workspace lock") = Some(worktree.clone());
        *self
            .narrow_git_common
            .lock()
            .expect("narrow git common lock") = Some(git_common.clone());
        // The restore closure reverts the narrow state + drops the .git
        // allow-back. It captures Arc clones of the shared Mutex fields so it
        // can revert through the original session (the Arc-clone points at the
        // same Mutex). Runs on explicit restore or Drop.
        let narrow_ws = Arc::clone(&self.narrow_workspace);
        let narrow_git = Arc::clone(&self.narrow_git_common);
        let additional_dirs = Arc::clone(&self.additional_dirs);
        let git_common_for_restore = git_common.clone();
        Ok(WorktreeFenceGuard::new(Box::new(move || {
            *narrow_ws.lock().expect("restore: narrow ws lock") = None;
            *narrow_git.lock().expect("restore: narrow git lock") = None;
            let mut dirs = additional_dirs.lock().expect("restore: additional lock");
            dirs.retain(|d| d != &git_common_for_restore);
            Ok(())
        })))
    }

    fn add_working_dir(&self, path: &str) -> Result<(), SandboxError> {
        // Canonicalize so mac /var -> /private/var symlinks and a trailing
        // slash do not create duplicate entries, and so the seatbelt subpath
        // matches the kernel's view of the path. Reject non-directory paths
        // (a file or a missing path grants nothing).
        let canonical = dunce::canonicalize(path)
            .map_err(|e| SandboxError::NotFound(format!("dir canonicalize: {path}: {e}")))?;
        if !canonical.is_dir() {
            return Err(SandboxError::NotFound(format!("not a directory: {path}")));
        }
        let mut dirs = self.additional_dirs.lock().expect("additional dirs lock");
        if dirs.iter().any(|d| d == &canonical) {
            return Ok(()); // idempotent
        }
        dirs.push(canonical);
        Ok(())
    }

    fn remove_working_dir(&self, path: &str) {
        // Match by canonical path when it resolves; if the path is gone now
        // (canonicalize fails), fall back to a string compare so a stale entry
        // for a deleted dir can still be cleared.
        let canonical = dunce::canonicalize(path).ok();
        let mut dirs = self.additional_dirs.lock().expect("additional dirs lock");
        dirs.retain(|d| match &canonical {
            Some(c) => d != c,
            None => !d.to_string_lossy().eq_ignore_ascii_case(path),
        });
    }

    fn working_dirs(&self) -> Vec<String> {
        self.additional_dirs
            .lock()
            .expect("additional dirs lock")
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect()
    }
}

/// RAII tree-kill guard. On Drop, killpg the whole process group. Instantiated
/// after spawn so EVERY exit path (Ok, Err, wall-timeout, future-drop/Ctrl-C)
/// reaps the tree. Without this, dropping the exec future only kill_on_drop'd
/// the direct child — re-introducing the orphan-process problem on the
/// cancel path. The guard makes the fix path-complete.
struct TreeKillGuard {
    pgid: i32,
}

impl TreeKillGuard {
    fn new(pgid: i32) -> Self {
        Self { pgid }
    }
}

impl Drop for TreeKillGuard {
    fn drop(&mut self) {
        kill_process_group(self.pgid);
    }
}

/// RAII guard pairing a SpawnEvent::Start so the matching End lands on EVERY
/// exit path, including future-drop/cancel (Ctrl-C). Without it, a cancel
/// between Start and the End call would leak the in-flight count and skip the
/// burned-CPU accounting, false-tripping the breaker after enough cancels.
/// On the normal path finish() is called with the pre-measured CPU so Drop
/// records those values; on the cancel path Drop measures the tree CPU
/// itself (the tree is still live — _tree_guard drops after this guard by
/// reverse declaration order, so the breaker measures before killpg reaps).
struct BreakerSpanGuard {
    breaker: Arc<ResourceBreaker>,
    pgid: i32,
    /// None until finish() sets it; None on Drop means the cancel path, so
    /// measure the tree CPU now (while it is still alive).
    cpu_secs: Option<u64>,
    exceeded_budget: bool,
}

impl BreakerSpanGuard {
    fn new(breaker: Arc<ResourceBreaker>, pgid: i32) -> Self {
        Self {
            breaker,
            pgid,
            cpu_secs: None,
            exceeded_budget: false,
        }
    }

    /// Record the measured outcome so Drop emits End with these values
    /// instead of measuring CPU itself. Called on the normal exit path.
    fn finish(&mut self, cpu_secs: u64, exceeded_budget: bool) {
        self.cpu_secs = Some(cpu_secs);
        self.exceeded_budget = exceeded_budget;
    }
}

impl Drop for BreakerSpanGuard {
    fn drop(&mut self) {
        let cpu_secs = self.cpu_secs.unwrap_or_else(|| tree_cpu_secs(self.pgid));
        self.breaker.record(SpawnEvent::End {
            cpu_secs,
            proc_count: 1,
            exceeded_budget: self.exceeded_budget,
        });
    }
}

#[cfg(test)]
#[path = "seatbelt_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "seatbelt_narrow_write_tests.rs"]
mod narrow_write_tests;

#[cfg(test)]
#[path = "seatbelt_git_config_tests.rs"]
mod git_config_tests;

#[cfg(test)]
#[path = "seatbelt_exec_tests.rs"]
mod exec_tests;
