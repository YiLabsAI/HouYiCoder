//! The sandbox execution port: the engine-facing contract for running
//! shell commands and file operations inside an OS-level fence. Signatures
//! reference context payload types (ExecConfig, ExecResult, DirEntry,
//! SandboxError). The concrete kernel-fenced backend (macOS Seatbelt today;
//! Linux bubblewrap/landlock/seccomp tracked follow-up) lives in the sandbox
//! crate; the engine depends on this trait so it does not depend on the
//! sandbox impl crate.
//!
//! Command interpretation is backend-defined: exec() takes a command string
//! whose shell and argv layout the backend selects (POSIX sh on Unix today).
//! A backend MUST NOT hard-wire a shell that is absent on the target platform
//! (no sh on bare Windows) — a future cross-platform backend picks its own
//! interpreter. The fence (process group + setrlimit + killpg + wall timeout)
//! is enforced per-call by the backend; a Tool overrides the per-call
//! ExecConfig.

use houyicoder_async::PFut;
use houyicoder_context::{DirEntry, ExecConfig, ExecResult, SandboxError};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// How wide the network fence is opened for a session.
///
/// Deliberately binary. There is no port-scoped middle tier: allowing outbound
/// port 443 reads as a narrowing but contains nothing, because anything worth
/// exfiltrating data to accepts connections on 443, while the tier breaks
/// legitimate traffic on every other port. Real egress containment requires a
/// proxy that resolves hostnames, which is a separate stage of this work. Until
/// that exists the posture is either contained or open, stated plainly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Egress {
    /// No outbound reachability. The kernel refuses the connection.
    #[default]
    Denied,
    /// Any host, any port. Not a contained posture and not presented as one.
    Unrestricted,
}

/// Which unix domain socket paths local IPC may use.
///
/// A separate axis from egress, because a unix socket is local IPC rather than
/// network reachability, yet the kernel classes both under the same operation
/// family: denying the network operation class also denies unix sockets. An ssh
/// agent, a container daemon, or a build daemon therefore needs an explicit
/// allow-back even when egress stays denied.
///
/// Modelled as an enum rather than a boolean + list, so that "allow
/// everything" and "allow these paths" cannot both be set.
/// In a boolean-only model the boolean silently wins over a
/// non-empty list, which makes a contradictory config look
/// effective while being ignored.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum UnixSockets {
    /// No unix socket use at all.
    #[default]
    Denied,
    /// Only these paths, each matched as a subpath.
    Paths(Vec<String>),
    /// Every path. Broad: includes sockets that themselves proxy to the
    /// network, so this can defeat a denied egress posture.
    All,
}

/// The network fence configuration for a session.
///
/// Every field's default is its narrowest value, so Default is full
/// containment and each opening is an explicit opt-in.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct NetworkPolicy {
    /// Outbound reachability.
    pub egress: Egress,
    /// Local IPC over unix domain sockets.
    pub unix_sockets: UnixSockets,
    /// Allow bind and accept on loopback, for a dev server or test harness
    /// started inside the fence. Grants no egress.
    pub allow_local_binding: bool,
}

impl NetworkPolicy {
    /// The fully contained policy: no egress, no unix sockets, no local bind.
    /// Named so call sites read as a posture rather than as a struct literal.
    #[must_use]
    pub fn contained() -> Self {
        Self::default()
    }

    /// True when the policy permits outbound reachability to arbitrary hosts.
    /// Callers that describe the posture to a user (the permission prompt, the
    /// status line) branch on this rather than matching the enum, so a future
    /// proxy tier changes one place.
    #[must_use]
    pub fn allows_egress(&self) -> bool {
        matches!(self.egress, Egress::Unrestricted)
    }
}

/// A backend-supplied restore action: reverts the fence to its pre-narrow
/// state. Captured by the guard so the api crate stays backend-agnostic.
pub type FenceRestore = Box<dyn FnOnce() -> Result<(), String> + Send + Sync>;

/// A guard that restores the sandbox fence to its pre-narrow state. Returned
/// by SandboxSession::narrow_to_worktree; Drop runs the restore best-effort
/// (logs on failure, never panics — AGENTS.md no-panics). For an explicit
/// restore with a real error, call restore() before dropping.
///
/// The restore action is backend-specific (the concrete session knows how it
/// narrowed + how to revert), so it is captured as a closure the backend
/// supplies. The guard is a concrete struct so Drop runs (a trait object Drop
/// would not fire).
pub struct WorktreeFenceGuard {
    restore: Mutex<Option<FenceRestore>>,
}

impl WorktreeFenceGuard {
    pub fn new(restore: FenceRestore) -> Self {
        Self {
            restore: Mutex::new(Some(restore)),
        }
    }

    /// Explicit restore on the happy path (worktree exit). Returns the
    /// backend error so the caller can surface it. Idempotent: a second call
    /// is a no-op (the restore closure ran once).
    pub fn restore(&self) -> Result<(), SandboxError> {
        let f = self.restore.lock().ok().and_then(|mut g| g.take());
        match f {
            Some(fn_) => fn_().map_err(SandboxError::Io),
            None => Ok(()),
        }
    }
}

impl Drop for WorktreeFenceGuard {
    fn drop(&mut self) {
        // Best-effort restore on drop (worktree exit without explicit restore,
        // or a controller panic). Never panic — a poisoned lock or a failed
        // restore logs and lets the process continue.
        if let Ok(mut g) = self.restore.lock()
            && let Some(fn_) = g.take()
            && let Err(e) = fn_()
        {
            tracing::warn!("worktree fence best-effort restore failed: {e}");
        }
    }
}

/// The side-effect level a tool produces. Ordered by ascending risk. The gate
/// uses this to decide which validators to run and, via the Containment trait,
/// whether the fence would block the call before the decision is made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SideEffect {
    /// Pure read, no observable mutation: read, view, ls, grep.
    None,
    /// Filesystem mutation: write, edit, multiedit.
    Filesystem,
    /// Network fetch: web fetch.
    Network,
    /// Shell execution: bash.
    Exec,
}

impl SideEffect {
    /// A short stable label for metrics buckets and audit records.
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Filesystem => "filesystem",
            Self::Network => "network",
            Self::Exec => "exec",
        }
    }
}

#[cfg(test)]
mod side_effect_tests {
    use super::SideEffect;

    #[test]
    fn test_label_round_trips() {
        assert_eq!(SideEffect::None.label(), "none");
        assert_eq!(SideEffect::Filesystem.label(), "filesystem");
        assert_eq!(SideEffect::Network.label(), "network");
        assert_eq!(SideEffect::Exec.label(), "exec");
    }
}

/// Whether a call is covered by the execution fence, and if so, which paths
/// the fence permits writes to. The gate constructs a FenceProof from this
/// (the proof token that allows an auto-allow under the containment contract);
/// the proof only needs the writable-root count, so this carries exactly that.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Coverage {
    /// The fence is active and covers the call; the writable roots are the
    /// paths inside the fence the agent may write.
    Fenced { writable_roots: Vec<PathBuf> },
    /// No fence is active (the session has no kernel sandbox).
    Unfenced,
}

/// The query interface a fence exposes to the permission gate BEFORE the gate
/// makes its decision. This is the contract that lets the gate know the fence
/// state without depending on the sandbox implementation crate: the gate holds
/// a Containment trait object and calls coverage() to build a proof +
/// would_block() to attach a note when the fence is expected to reject the
/// call even after consent.
///
/// A per-command boolean is the simpler shape; this enum is richer:
/// coverage() returns the writable roots (not just a flag), and
/// would_block() is a pre-decision query (a boolean-only model lacks
/// this — its fence blocks at execution time; the gate never sees it).
pub trait Containment: Send + Sync {
    /// What the fence covers right now. The gate builds a FenceProof from this
    /// when the side effect is Exec (the proof token's eligibility check).
    fn coverage(&self) -> Coverage;

    /// Whether the fence would reject this side effect, and a human-readable
    /// note explaining why. None when the fence permits the effect. The gate
    /// attaches this as containment_note on an Ask so the user knows the fence
    /// will block even if they approve -- information, not a rejection.
    fn would_block(&self, effect: SideEffect) -> Option<String>;

    /// The canonical workspace root the fence is bound to. None when the
    /// fence is not narrowed or is a stub — a gate-side path-bounds check then
    /// degrades to conservative (the gate does not guess in-bounds; the tool's
    /// confine_path is the backstop). Default None so a non-fence Containment
    /// impl stays unchanged without knowing the root.
    fn boundary_root(&self) -> Option<Arc<Path>> {
        None
    }

    /// The canonical additional authorized dirs (the fence's working_dirs as
    /// PathBuf). Default empty. The gate pairs this with workspace_root and
    /// is_within_bounds to decide whether a grep/glob path is outside and
    /// should Ask; the fence is the authority, the gate only asks.
    fn boundary_dirs(&self) -> Vec<PathBuf> {
        Vec::new()
    }
}

/// The single workspace-boundary predicate: a canonical candidate is within
/// bounds when it is under the canonical root or any additional authorized
/// dir. Shared by confine_path (tool execution) and the gate (pre-check ask)
/// so the two layers cannot drift on what "inside the workspace" means. The
/// network-posture incident taught that a second authority drifts: the gate
/// asks when this returns false, it does not itself judge in-bounds (an
/// uncertain canonicalize or a missing fence degrades to "do not ask", letting
/// confine_path / the kernel fence enforce).
pub fn is_within_bounds(candidate: &Path, root: &Path, additional: &[PathBuf]) -> bool {
    candidate.starts_with(root) || additional.iter().any(|d| candidate.starts_with(d))
}

/// Extract the path-bearing args from grep/glob input for a boundary check.
/// grep contributes its path; glob contributes its path and the directory
/// portion of its pattern (the part that can escape via parent-dir segments).
/// Other tools return empty. Shared by the gate's pre-check ask and the
/// server's consent routing so the two layers cannot drift on which field is
/// the path. The caller canonicalizes + applies is_within_bounds.
pub fn path_args_for_boundary(tool_name: &str, input: Option<&serde_json::Value>) -> Vec<String> {
    let Some(v) = input else {
        return Vec::new();
    };
    match tool_name.to_ascii_lowercase().as_str() {
        "grep" => v
            .get("path")
            .and_then(|x| x.as_str())
            .map(|s| vec![s.to_string()])
            .unwrap_or_default(),
        "glob" => {
            let mut out = Vec::new();
            if let Some(p) = v.get("path").and_then(|x| x.as_str()) {
                out.push(p.to_string());
            }
            if let Some(pat) = v.get("pattern").and_then(|x| x.as_str()) {
                // Truncate at the first wildcard char so the dir portion
                // canonicalizes cleanly (../cc-bck/**/*.rs → ../cc-bck/). This
                // mirrors the glob tool's own check_pattern_confined logic so
                // the gate's pre-check + the tool's enforcement agree on what
                // the "dir portion" is.
                let prefix = match pat.find(['*', '?', '[']) {
                    Some(pos) => &pat[..pos],
                    None => pat,
                };
                let dir = prefix.rfind('/').map(|i| &prefix[..i]).unwrap_or(prefix);
                let dir = dir.trim_end_matches('/');
                if !dir.is_empty() {
                    out.push(dir.to_string());
                }
            }
            out
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod containment_tests {
    use super::{Containment, Coverage, SideEffect};
    use std::path::PathBuf;

    /// A stub fence: Fenced with one root, blocks Network, permits everything
    /// else. Pins the trait's contract before any real backend implements it,
    /// so the real impl lights up an existing expectation rather than bringing
    /// one in.
    struct StubFence;

    impl Containment for StubFence {
        fn coverage(&self) -> Coverage {
            Coverage::Fenced {
                writable_roots: vec![PathBuf::from("/ws")],
            }
        }
        fn would_block(&self, effect: SideEffect) -> Option<String> {
            match effect {
                SideEffect::Network => Some("egress is contained".into()),
                _ => None,
            }
        }
    }

    /// The trait must be dyn-safe (the gate holds a trait object).
    fn _assert_dyn_safe(_: &dyn Containment) {}

    #[test]
    fn test_stub_reports_fenced_coverage() {
        let f = StubFence;
        assert!(matches!(f.coverage(), Coverage::Fenced { .. }));
    }

    /// The trait's boundary_root + boundary_dirs default to None/empty when a
    /// Containment impl does not override them (a stub or an unfenced fence).
    /// Pins the defaults so a non-fence impl stays unchanged without knowing
    /// the root, and the gate degrades to "do not ask" (no fence info).
    #[test]
    fn test_boundary_defaults_none_empty() {
        let f = StubFence;
        assert!(f.boundary_root().is_none(), "default boundary_root is None");
        assert!(
            f.boundary_dirs().is_empty(),
            "default boundary_dirs is empty"
        );
    }

    #[test]
    fn test_stub_blocks_network_only() {
        let f = StubFence;
        assert!(f.would_block(SideEffect::Network).is_some());
        assert!(f.would_block(SideEffect::None).is_none());
    }

    #[test]
    fn test_stub_dyn_safe() {
        let f = StubFence;
        _assert_dyn_safe(&f);
    }
}

/// The kernel fence status after session construction. The composition root
/// checks this once to surface a user-visible notice when the fence did not
/// engage — the user must know their workspace is running unfenced, not learn
/// it from a diagnostic log they never open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FenceStatus {
    /// The kernel fence is active (Landlock enforced, Job Object assigned,
    /// seatbelt applied). Operations are confined.
    Enforced,
    /// The fence was attempted but the kernel did not enforce it (older
    /// kernel, unsupported ABI). Operations run unfenced.
    NotEnforced,
    /// The fence apply call itself failed (a system error). Operations run
    /// unfenced.
    Failed(String),
    /// No kernel fence exists for this platform/build (the stub backend,
    /// or enforce feature off). Operations run with path-only confinement.
    Unavailable,
}

impl FenceStatus {
    /// True when the kernel fence is NOT active — the user should be told.
    pub fn is_unfenced(&self) -> bool {
        !matches!(self, FenceStatus::Enforced)
    }

    /// A one-line human-readable notice for the unfenced case, or None when
    /// the fence is enforced.
    pub fn unfenced_notice(&self) -> Option<String> {
        match self {
            FenceStatus::Enforced => None,
            FenceStatus::NotEnforced => {
                Some("sandbox fence not enforced by the kernel; running unfenced".into())
            }
            FenceStatus::Failed(e) => {
                Some(format!("sandbox fence apply failed: {e}; running unfenced"))
            }
            FenceStatus::Unavailable => {
                Some("no kernel sandbox fence on this platform; running unfenced".into())
            }
        }
    }
}

/// The sandbox execution primitive a Tool wraps. Object-safe (PFut) so the
/// engine holds an Arc<dyn SandboxSession> and the concrete backend swaps
/// behind it. A session owns a workspace root (temp dir, user cwd, or a
/// linked git worktree); every operation is confined to that root by both an
/// application-level path resolver and the kernel fence (the kernel fence is
/// the real boundary).
pub trait SandboxSession: Send + Sync {
    /// The kernel fence status after construction. The composition root
    /// checks this once to surface a user-visible notice when the fence
    /// did not engage. Default returns Unavailable for backends that do
    /// not override (the stub).
    fn fence_status(&self) -> FenceStatus {
        FenceStatus::Unavailable
    }

    /// Downcast to the Containment query interface when the backend has a
    /// real fence. MacSeatbeltSession returns Some(self); stub backends
    /// return None. This avoids the double-trait-object problem: the gate
    /// holds an Arc<dyn SandboxSession> and asks for Containment through
    /// this method rather than trying to cast between two trait objects.
    fn as_containment(&self) -> Option<&dyn Containment> {
        None
    }

    /// Run a command inside the fence at the workspace root with the default
    /// resource fence (ExecConfig::default). stdout/stderr/exit_code are
    /// captured; the command cannot reach paths outside the workspace plus the
    /// system read-allowlist, and cannot reach the network unless the session was
    /// configured with a policy that opens it. Command interpretation is
    /// backend-defined (see module docs).
    fn exec(&self, command: &str) -> PFut<'_, Result<ExecResult, SandboxError>> {
        self.exec_with_config(command, ExecConfig::default())
    }

    /// Run a command with an explicit per-call resource fence. The fence:
    /// process_group + kill_on_drop + setrlimit (CPU/AS/NPROC) + wall-clock
    /// timeout + killpg whole-tree on breach. A Tool overrides per-call (e.g.
    /// a long bench run widens cpu_secs + wall). Backends without a kernel
    /// fence return Unsupported.
    fn exec_with_config(
        &self,
        command: &str,
        config: ExecConfig,
    ) -> PFut<'_, Result<ExecResult, SandboxError>>;

    /// Run a command that streams its stdout line count to a shared counter
    /// while it executes, so a host can show "(12s · 14 lines)" on a
    /// long-running bash chip. The counter is an AtomicI64 the backend
    /// updates as stdout chunks arrive; -1 means "no output yet / not
    /// streaming" (the host shows no line count), 0+ is the running newline
    /// count. Default delegates to exec (no streaming, counter untouched) —
    /// only streaming backends override. Same fence + timeout + tree-kill
    /// invariants as exec_with_config.
    fn exec_streaming(
        &self,
        command: &str,
        _lines: std::sync::Arc<std::sync::atomic::AtomicI64>,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        self.exec(command)
    }

    /// Emit an audit line when the kernel fence is not active, so the
    /// unfenced gap is visible per-operation. Bound to fence_status(),
    /// not to "remember to write the line in each impl" — any backend
    /// reporting an unfenced status (NotEnforced / Failed / Unavailable)
    /// automatically audits, and a new backend that forgets to override
    /// inherits the default and still audits. op is the per-call context.
    fn audit_unfenced(&self, op: &str) {
        if let Some(notice) = self.fence_status().unfenced_notice() {
            tracing::warn!("sandbox audit [{op}]: {notice}");
        }
    }

    /// Read a file under the workspace, up to max_bytes. Paths outside the
    /// workspace are refused (application-level guard supplements the kernel
    /// fence; the kernel fence is the real boundary). Default resolves the
    /// path then reads, auditing first when the fence is unfenced; backends
    /// with a different resolver override resolve().
    fn read_file(&self, path: &str, max_bytes: usize) -> PFut<'_, Result<Vec<u8>, SandboxError>> {
        let resolved = match self.resolve(path) {
            Ok(p) => p,
            Err(e) => return Box::pin(async move { Err(e) }),
        };
        Box::pin(async move {
            self.audit_unfenced("read");
            let bytes = std::fs::read(&resolved)?;
            if bytes.len() > max_bytes {
                Ok(bytes[..max_bytes].to_vec())
            } else {
                Ok(bytes)
            }
        })
    }

    /// Write bytes to a file under the workspace. Path escapes are refused by
    /// the same resolve() guard as read_file; the kernel fence is the real
    /// boundary. Takes owned bytes so an async backend moves them without clone.
    fn write_file(&self, path: &str, content: Vec<u8>) -> PFut<'_, Result<(), SandboxError>> {
        let resolved = match self.resolve(path) {
            Ok(p) => p,
            Err(e) => return Box::pin(async move { Err(e) }),
        };
        Box::pin(async move {
            self.audit_unfenced("write");
            if let Some(parent) = resolved.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&resolved, content)?;
            Ok(())
        })
    }

    /// List a directory under the workspace.
    fn list_dir(&self, path: &str) -> PFut<'_, Result<Vec<DirEntry>, SandboxError>> {
        let resolved = match self.resolve(path) {
            Ok(p) => p,
            Err(e) => return Box::pin(async move { Err(e) }),
        };
        Box::pin(async move {
            self.audit_unfenced("list");
            let mut entries = Vec::new();
            for entry in std::fs::read_dir(&resolved)? {
                let entry = entry?;
                entries.push(DirEntry {
                    name: entry.file_name().to_string_lossy().into_owned(),
                    is_dir: entry.file_type().map(|t| t.is_dir()).unwrap_or(false),
                });
            }
            Ok(entries)
        })
    }

    /// True when the path exists under the workspace.
    fn path_exists(&self, path: &str) -> PFut<'_, Result<bool, SandboxError>> {
        let resolved = match self.resolve(path) {
            Ok(p) => p,
            Err(e) => return Box::pin(async move { Err(e) }),
        };
        Box::pin(async move {
            self.audit_unfenced("exists");
            Ok(resolved.exists())
        })
    }

    /// The workspace root (a temp dir, user cwd, or linked worktree owned by
    /// this session). Returns an owned Arc so a runtime fence swap (worktree
    /// enter narrows the root; exit restores it) can change the reported root
    /// without a borrow tied to self — a &Path return cannot outlive an
    /// interior-mutable swap.
    fn workspace_root(&self) -> Arc<Path>;

    /// Resolve a logical path against the workspace root, refusing escapes
    /// (absolute paths are rejected; .. is collapsed and must stay inside).
    /// The application guard supplements the kernel fence; under the no-op
    /// stub this resolver is the only boundary. Default joins against
    /// workspace_root() and canonicalizes via dunce (no UNC prefix on
    /// Windows, so starts_with comparisons hold). workspace_root() must
    /// return a canonical path (backends canonicalize at construction) — the
    /// canonicalized resolved path is compared against it, so a non-canonical
    /// root (e.g. a raw temp_dir() symlink on macOS) would falsely fail.
    /// Backends that authorize external dirs (mac seatbelt's add_working_dir)
    /// override to admit absolute paths landing in an allowed root.
    fn resolve(&self, path: &str) -> Result<PathBuf, SandboxError> {
        let workspace = self.workspace_root();
        let p = Path::new(path);
        if p.is_absolute() {
            return Err(SandboxError::PathTraversal(format!(
                "path must be workspace-relative, got absolute: {path}"
            )));
        }
        let joined = workspace.join(path);
        let canonical = joined
            .parent()
            .filter(|p| p.exists())
            .and_then(|p| dunce::canonicalize(p).ok())
            .map(|parent| parent.join(joined.file_name().unwrap_or_default()))
            .unwrap_or(joined.clone());
        if !canonical.starts_with(&*workspace) {
            return Err(SandboxError::PathTraversal(format!(
                "path escapes workspace: {path}"
            )));
        }
        Ok(canonical)
    }

    /// Narrow the fence to a linked worktree plus allow-back the repo .git so
    /// git ops can read/write objects and refs (the main working tree stays
    /// fenced out — the kernel fence is the real isolation). The worktree
    /// profile also read-allows .git/config (git needs repo config) while
    /// keeping the write deny (blocks credential-helper insertion). Returns a
    /// guard whose Drop restores the pre-narrow fence. Backends without a
    /// runtime-mutable fence return Unsupported.
    fn narrow_to_worktree(
        &self,
        _worktree: &Path,
        _git_common_dir: &Path,
    ) -> Result<WorktreeFenceGuard, SandboxError> {
        Err(SandboxError::Unsupported(
            "runtime fence narrowing needs a mutable backend".into(),
        ))
    }

    /// The count of in-flight exec spawns. A worktree enter refuses while this
    /// is non-zero (a long-running spawn keeps its wide-fence profile alive
    /// and could still write the main tree after the narrow). Backends that do
    /// not track in-flight spawns return 0 (the worktree enter then trusts the
    /// caller to have no backgrounded commands).
    fn active_exec_count(&self) -> usize {
        0
    }

    /// Add a directory the sandboxed agent may touch beyond the workspace
    /// root. The path is canonicalized + validated as a directory by the
    /// backend; the kernel fence (the seatbelt profile's allow-back) is
    /// re-derived so the next exec sees it. Idempotent — adding a dir twice
    /// is the same as once. Backends without a runtime-mutable fence return
    /// Unsupported (the directory is not granted).
    fn add_working_dir(&self, _path: &str) -> Result<(), SandboxError> {
        Err(SandboxError::Unsupported(
            "additional working dirs need a runtime-mutable sandbox fence".into(),
        ))
    }

    /// Remove a previously-added directory. No-op when the path was never
    /// added. Backends without a runtime-mutable fence silently no-op.
    fn remove_working_dir(&self, _path: &str) {}

    /// The canonicalized paths of the directories added at runtime (not
    /// counting the workspace root). Empty when the backend has no
    /// runtime-mutable fence or none have been added.
    fn working_dirs(&self) -> Vec<String> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The trait must support runtime dispatch.
    #[test]
    fn test_trait_is_object_safe() {
        let _session: Box<dyn SandboxSession> = Box::new(Stub);
    }

    /// Default working-dir impls: a backend that does not override them
    /// returns Unsupported on add, no-ops on remove, and an empty list.
    /// Backends with a mutable fence override all three.
    #[test]
    fn test_default_working_dir_unsupported() {
        let s = Stub;
        assert!(matches!(
            s.add_working_dir("/tmp"),
            Err(SandboxError::Unsupported(_))
        ));
        s.remove_working_dir("/tmp"); // no-op, no panic
        assert!(s.working_dirs().is_empty());
    }

    /// The fence guard runs the restore closure on explicit restore(), and
    /// again on Drop (best-effort) — the second call is a no-op. A guard
    /// dropped WITHOUT explicit restore also runs the closure (best-effort
    /// path). Covers the Drop branch + the idempotent second-call path.
    #[test]
    fn test_fence_guard_restore_drop() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let count = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&count);
        let guard = WorktreeFenceGuard::new(Box::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }));
        assert!(guard.restore().is_ok());
        assert_eq!(count.load(Ordering::SeqCst), 1, "explicit restore ran once");
        drop(guard); // Drop best-effort — closure already taken, no-op.
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "drop after restore is a no-op"
        );

        // A guard dropped WITHOUT restore: Drop runs the closure.
        let c2 = Arc::clone(&count);
        let guard2 = WorktreeFenceGuard::new(Box::new(move || {
            c2.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }));
        drop(guard2);
        assert_eq!(
            count.load(Ordering::SeqCst),
            2,
            "drop without restore runs the closure"
        );
    }

    struct Stub;
    impl SandboxSession for Stub {
        fn exec_with_config(
            &self,
            _command: &str,
            _config: ExecConfig,
        ) -> PFut<'_, Result<ExecResult, SandboxError>> {
            Box::pin(async move {
                Ok(ExecResult {
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: Some(0),
                })
            })
        }
        fn read_file(
            &self,
            _path: &str,
            _max_bytes: usize,
        ) -> PFut<'_, Result<Vec<u8>, SandboxError>> {
            Box::pin(async move { Ok(Vec::new()) })
        }
        fn write_file(&self, _path: &str, _content: Vec<u8>) -> PFut<'_, Result<(), SandboxError>> {
            Box::pin(async move { Ok(()) })
        }
        fn list_dir(&self, _path: &str) -> PFut<'_, Result<Vec<DirEntry>, SandboxError>> {
            Box::pin(async move { Ok(Vec::new()) })
        }
        fn path_exists(&self, _path: &str) -> PFut<'_, Result<bool, SandboxError>> {
            Box::pin(async move { Ok(false) })
        }
        fn workspace_root(&self) -> Arc<Path> {
            Arc::from(std::path::PathBuf::from("/"))
        }
    }

    /// Enforced is the only status that is not unfenced; the other three
    /// all carry a user-visible notice. The notice is the composition root's
    /// signal to queue a startup system line.
    #[test]
    fn test_fence_status_unfenced_notice() {
        assert!(!FenceStatus::Enforced.is_unfenced(), "Enforced is fenced");
        assert!(FenceStatus::Enforced.unfenced_notice().is_none());
        for status in [
            FenceStatus::NotEnforced,
            FenceStatus::Failed("landlock error".into()),
            FenceStatus::Unavailable,
        ] {
            assert!(status.is_unfenced(), "{status:?} is unfenced");
            let notice = status.unfenced_notice().expect("unfenced has a notice");
            assert!(
                notice.contains("unfenced"),
                "notice mentions unfenced: {notice}"
            );
        }
        // The Failed variant carries the error detail.
        assert!(
            FenceStatus::Failed("landlock error".into())
                .unfenced_notice()
                .unwrap()
                .contains("landlock error"),
            "Failed notice carries the error"
        );
    }

    /// The default fence_status (for backends that do not override) is
    /// Unavailable — the stub backend has no kernel fence.
    #[test]
    fn test_fence_status_default_unavailable() {
        let stub = Stub;
        assert_eq!(
            stub.fence_status(),
            FenceStatus::Unavailable,
            "default is Unavailable for backends without a real fence"
        );
    }
}

#[cfg(test)]
mod default_tests;
