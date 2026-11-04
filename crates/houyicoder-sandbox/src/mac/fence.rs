//! The fence contract: what the session contains, how wide it is, and the
//! capability grants a skill or a worktree may add or revoke.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;

use houyicoder_api::capture::RawOutputReceiver;
use houyicoder_api::sandbox::{
    Containment, Coverage, FenceCapability, FenceStatus, SandboxSession, SideEffect,
    WorktreeFenceGuard,
};
use houyicoder_async::PFut;
use houyicoder_context::{ExecConfig, ExecResult, SandboxError};

use super::MacSeatbeltSession;
use super::worktree;

/// The fence status for a session given whether the no-enforce hatch is set.
/// Separated from the trait method so the decision is testable without
/// manipulating the process environment.
pub(super) fn fence_status_for(unfenced: bool) -> FenceStatus {
    if unfenced {
        FenceStatus::NotEnforced
    } else {
        FenceStatus::Enforced
    }
}

impl MacSeatbeltSession {
    /// Record that the fence now contains a different set of paths. Only a
    /// grant that actually changed the set advances the counter, so a
    /// repeated install does not look like a fresh capability.
    pub(super) fn note_capability_change(&self) {
        self.revision.fetch_add(1, Ordering::SeqCst);
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

impl SandboxSession for MacSeatbeltSession {
    /// Enforced whenever the session exists and the fence is not bypassed by
    /// the no-enforce escape hatch. When the hatch is set, commands run through
    /// the shell directly and the status reports as not enforced so tests that
    /// assert kernel-level denials skip instead of running unfenced.
    fn fence_status(&self) -> FenceStatus {
        fence_status_for(crate::is_unfenced())
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
    /// newlines arrive, and the same bytes feed the caller's receiver. Same
    /// fence + timeout + tree-kill invariants as exec_with_config; only the
    /// counter differs.
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

    /// The shared temp root is writable through this profile, so a tool writing
    /// there is inside the fence and the gate has nothing to ask about. A
    /// handful of subtrees inside it stay denied to protect other processes,
    /// which the kernel enforces on its own; asking here would add nothing.
    ///
    /// The root has two spellings and they name one directory, the shorter
    /// being a link to the longer. Both are tried because either may arrive in
    /// a tool's path, and the answer is reduced to canonical form so that one
    /// directory appears once in the bounds the gate reads.
    fn shared_temp_roots(&self) -> Vec<PathBuf> {
        let mut roots: Vec<PathBuf> = Vec::new();
        for spelling in ["/tmp", "/private/tmp"] {
            let path = PathBuf::from(spelling);
            if !path.is_dir() {
                continue;
            }
            let identity = dunce::canonicalize(&path).unwrap_or(path);
            if !roots.contains(&identity) {
                roots.push(identity);
            }
        }
        roots
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
