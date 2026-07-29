//! Recoverable destructive operations: workspace copy-on-write snapshots
//! and an undo stack. A destructive bash command snapshots the whole
//! workspace tree (reflink per file, O(file count)) before executing; /undo
//! restores from the snapshot. File-tool per-file before-images (Write/Edit/
//! MultiEdit) share the same undo stack (the per-file before-image design).
//!
//! The snapshot store is workspace-local (the workspace-local snapshot store)
//! because clonefile/FICLONE require same-volume (a global store may cross a
//! volume boundary -> EXDEV). The store directory itself is pruned from the
//! walkdir so a snapshot does not clone prior snapshots (self-referential
//! explosion).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use walkdir::WalkDir;

/// Directories excluded from a snapshot: the snapshot store itself (prevents
/// self-referential explosion) plus large generated trees.
const PRUNE_DIRS: &[&str] = &[".houyicoder", ".claude", "target", "node_modules", ".git"];

/// A simplified destructive-command check (the snapshot trigger predicate).
/// The authoritative check (with compound splitting + attestability) lives in
/// the permission crate's should_ask_destructive, which the gate uses for the
/// Ask/Allow decision. This heuristic is for the snapshot trigger only: a
/// false negative (misses a destructive command) is safe because the gate
/// still asks; a false positive (snapshots a non-destructive command) wastes
/// a snapshot but is harmless. The layering forbids core from importing the
/// permission crate (permission depends on core, not vice versa), so this is
/// a self-contained keyword check.
pub fn is_destructive_command(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    // rm / rmdir / unlink — file deletion.
    let has_rm = lower
        .split_whitespace()
        .any(|tok| tok == "rm" || tok == "rmdir" || tok == "unlink" || tok.starts_with("rm"));
    // sudo — privilege escalation (whatever follows is elevated).
    let has_sudo = lower.split_whitespace().any(|tok| tok == "sudo");
    // Redirect operators (unquoted) — output redirection can overwrite.
    let has_redirect = {
        let in_single = lower.contains('\'');
        let in_double = lower.contains('"');
        !in_single && !in_double && (lower.contains('>') || lower.contains(">>"))
    };
    // Git working-tree / history discards (checkout -- / path / -f, restore,
    // clean -f, stash drop/clear/pop, branch -d/-D, push --force) — back up
    // the tree before they run so /undo can recover. Argument-aware so a bare
    // git checkout <branch> switch does NOT trigger a snapshot. Matches
    // the consent-gate classifier in the permission crate; keep both in
    // sync.
    let has_git_discard = crate::agent::git_discard::command_triggers_git_snapshot(command);
    has_rm || has_sudo || has_redirect || has_git_discard
}

/// One undo entry on the LIFO stack. Bash commands produce a whole-workspace
/// CoW snapshot (touch-set unknowable); file tools produce a per-file
/// before-image (touch-set declared, cheap — the per-file before-image design).
#[derive(Debug)]
pub enum UndoEntry {
    CoWSnapshot {
        store_path: PathBuf,
        /// Additional writable roots snapshotted alongside the workspace
        /// (user-added working dirs). Stored so restore can map the
        /// __extra/<i>/ sub-tree back to its original root. Empty for a
        /// workspace-only snapshot.
        extra_roots: Vec<PathBuf>,
    },
    BeforeImage {
        path: PathBuf,
        before: Option<Vec<u8>>,
    },
}

impl UndoEntry {
    /// A human-readable description of what was undone, for the /undo reply.
    /// Does not oversell: says "restored N files from snapshot" (honest about
    /// the full-restore semantics — new files created after the snapshot
    /// are NOT deleted, .git is pruned from snapshots, symlinks are skipped).
    pub fn description(&self) -> String {
        match self {
            UndoEntry::CoWSnapshot { store_path, .. } => {
                let count = count_restored_files(store_path);
                format!("restored {count} files from snapshot")
            }
            UndoEntry::BeforeImage { .. } => "restored file from before-image".into(),
        }
    }
}

/// The outcome of an /undo attempt. Distinguishes empty stack, success, and
/// restore failure — the caller (server handler) surfaces each to the user
/// so a restore failure is never confused with an empty stack.
#[derive(Debug)]
pub enum UndoOutcome {
    /// The undo stack is empty — nothing to undo.
    Empty,
    /// Restore succeeded; the entry describes what was undone.
    Restored(UndoEntry),
    /// Restore failed; the entry is still on the stack (not popped) so the
    /// user can retry. The string is the error message for the user.
    Failed(String),
}

/// Count files in a snapshot directory (for the honest "restored N files"
/// reply — does not claim the whole workspace was restored).
fn count_restored_files(snap_path: &Path) -> usize {
    WalkDir::new(snap_path)
        .min_depth(1)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .count()
}

/// The undo stack: strict LIFO. /undo pops the most recent entry.
#[derive(Debug, Default)]
pub struct UndoStack {
    entries: Vec<UndoEntry>,
}

impl UndoStack {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, entry: UndoEntry) {
        self.entries.push(entry);
    }

    pub fn pop(&mut self) -> Option<UndoEntry> {
        self.entries.pop()
    }

    /// Peek the top entry without removing it. Used by undo_last to
    /// restore first, then pop only on success — a restore failure
    /// leaves the entry on the stack for retry.
    pub fn peek(&self) -> Option<&UndoEntry> {
        self.entries.last()
    }

    /// Paths referenced by CoWSnapshot entries on the stack — prune must
    /// skip these so it never deletes a snapshot the undo stack points at.
    pub fn snapshot_paths(&self) -> Vec<PathBuf> {
        self.entries
            .iter()
            .filter_map(|e| match e {
                UndoEntry::CoWSnapshot { store_path, .. } => Some(store_path.clone()),
                _ => None,
            })
            .collect()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Rebuild the stack from a re-link scan of surviving on-disk snapshots
    /// (after a restart/resume). The on-disk snap-N dirs persist across
    /// processes; only the in-memory stack is lost. Entries must be in LIFO
    /// order (most recent first) -- the caller (relink_undo_entries) sorts
    /// by snap id descending.
    pub fn from_entries(entries: Vec<UndoEntry>) -> Self {
        Self { entries }
    }
}

/// The default threshold (bytes) above which a full-copy fallback on a
/// no-reflink filesystem degrades to Ask instead of paying a slow copy per
/// destructive command.
const NO_REFLINK_SIZE_THRESHOLD: u64 = 512 * 1024 * 1024;

/// The workspace-local snapshot store. Snapshots are whole-tree CoW clones
/// (reflink per file + create_dir), O(file count). The store directory itself
/// is pruned from the walk.
pub struct SnapshotStore {
    workspace_root: PathBuf,
    store_root: PathBuf,
    counter: AtomicU64,
    audit: Option<Arc<dyn SnapshotAuditSink>>,
    /// Cached once per process: whether the workspace filesystem supports
    /// copy-on-write reflink. Probed by writing + reflinking + deleting two
    /// files, so repeated calls are a real cost on the hot path. The answer
    /// is constant for a store's lifetime (the filesystem does not change
    /// under a running session), so it is memoized.
    reflink_capable: OnceLock<bool>,
}

/// Audit sink for snapshot lifecycle events. Injected via
/// SnapshotStore::with_audit; None means no audit. The production adapter
/// forwards into the session event log (SessionLog::append); tests inject a
/// recording sink. Kept as a trait so SnapshotStore stays decoupled from the
/// trajectory/event-log types.
pub trait SnapshotAuditSink: Send + Sync {
    /// A snapshot was created and is about to be pushed on the undo stack.
    fn snapshot_created(&self, entry: &UndoEntry);
    /// N snapshots were pruned by prune(ttl, max_size, protected).
    fn snapshot_pruned(&self, removed: usize);
    /// An undo entry was restored (undo applied).
    fn undo_applied(&self, entry: &UndoEntry);
}

impl SnapshotStore {
    pub fn new(workspace_root: impl AsRef<Path>) -> std::io::Result<Self> {
        let workspace_root = workspace_root.as_ref().to_path_buf();
        let store_root = workspace_root.join(".houyicoder").join("snapshots");
        std::fs::create_dir_all(&store_root)?;
        // Scan existing snap-N dirs to avoid id collision across restarts.
        // The store dir persists across processes; a fresh counter at 0
        // would land new snap-0 on top of old snap-0.
        let start = scan_max_snapshot_id(&store_root) + 1;
        Ok(Self {
            workspace_root,
            store_root,
            counter: AtomicU64::new(start),
            audit: None,
            reflink_capable: OnceLock::new(),
        })
    }

    /// Attach an audit sink. The store forwards snapshot_created /
    /// snapshot_pruned / undo_applied as lifecycle events happen. None by
    /// default; the composition root wires a SessionLog-adapter in production.
    pub fn with_audit(mut self, sink: Arc<dyn SnapshotAuditSink>) -> Self {
        self.audit = Some(sink);
        self
    }

    /// Reconstruct undo entries from the surviving on-disk snap-N dirs, so a
    /// resumed session can /undo a destructive operation from the prior
    /// process. The snapshots persist on disk across restarts; only the
    /// in-memory stack is lost. Returns entries in LIFO order (highest snap
    /// id first = most recent destructive op on top).
    ///
    /// Workspace-only snapshots re-link cleanly (restore maps the cloned tree
    /// back to the workspace root). Snapshots that carried additional writable
    /// roots (an __extra subtree) are skipped: the original extra_roots
    /// mapping is not on disk, so a restore could not remap the extra
    /// subtrees back to their roots. Restoring those needs the sidecar (a
    /// future refinement); today they are left on disk, unrecoverable via
    /// /undo after a restart, the same as before this re-link.
    pub fn relink_undo_entries(&self) -> Vec<UndoEntry> {
        let mut ids: Vec<u64> = Vec::new();
        if let Ok(dir) = std::fs::read_dir(&self.store_root) {
            for entry in dir.flatten() {
                let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                if !is_dir {
                    continue;
                }
                let file_name = entry.file_name();
                let Some(name) = file_name.to_str() else {
                    continue;
                };
                let Some(num) = name.strip_prefix("snap-") else {
                    continue;
                };
                let Ok(n) = num.parse::<u64>() else {
                    continue;
                };
                let snap_path = self.store_root.join(name);
                // Skip additional-roots snapshots: the __extra subtree's
                // root mapping is not on disk, so restore could not remap.
                if snap_path.join("__extra").exists() {
                    continue;
                }
                ids.push(n);
            }
        }
        ids.sort_unstable();
        ids.iter()
            .rev()
            .map(|&n| UndoEntry::CoWSnapshot {
                store_path: self.store_root.join(format!("snap-{n}")),
                extra_roots: Vec::new(),
            })
            .collect()
    }

    /// Whether CoW reflink is available on the workspace's filesystem. Tries
    /// a real reflink (not the copy-fallback) on a temp file pair in the
    /// store directory. On APFS/btrfs/XFS this succeeds; on ext4 it errors.
    /// Memoized: the filesystem does not change under a running session, and
    /// the probe writes + reflinks + deletes two files on every call, so the
    /// hot path (a destructive command probes once per snapshot) pays it
    /// once per process rather than once per command.
    pub fn can_reflink(&self) -> bool {
        *self.reflink_capable.get_or_init(|| {
            let src = self.store_root.join(".reflink-test-src");
            let dst = self.store_root.join(".reflink-test-dst");
            if std::fs::write(&src, b"t").is_err() {
                return false;
            }
            let ok = reflink_copy::reflink(&src, &dst).is_ok();
            let _r1 = std::fs::remove_file(&src);
            let _r2 = std::fs::remove_file(&dst);
            ok
        })
    }

    /// Total size (bytes) of the prunable workspace tree, excluding the
    /// snapshot store + PRUNE_DIRS. Used by the ext4 fallback policy: when
    /// reflink is unavailable and the workspace exceeds the threshold, the
    /// caller degrades to Ask rather than paying a full copy.
    pub fn workspace_size(&self) -> u64 {
        let mut total: u64 = 0;
        for entry in WalkDir::new(&self.workspace_root)
            .min_depth(1)
            .into_iter()
            .filter_entry(|e| !is_pruned(e.path(), &self.workspace_root, &self.store_root))
            .flatten()
        {
            if entry.file_type().is_file() {
                total += entry.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
        total
    }

    /// Check the ext4 fallback policy: if CoW is unavailable AND the workspace
    /// exceeds the threshold, return an error so the caller skips the snapshot
    /// rather than paying a slow full copy. A decline is a performance skip,
    /// not a safety event -- undo is possible but deliberately not paid for,
    /// so the caller runs the command and surfaces that undo is unavailable
    /// for it. On APFS/btrfs/XFS (reflink available) this always returns Ok.
    /// A real I/O failure from the walk, by contrast, is undo genuinely
    /// unavailable and the caller refuses to run.
    pub fn check_reflink_policy(&self) -> std::io::Result<()> {
        reflink_policy(self.can_reflink(), self.workspace_size())
    }

    /// The workspace root (the snapshot scope).
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    /// Snapshot the whole workspace tree (CoW per file) into a new snapshot
    /// dir, returning the UndoEntry for it. Prunes the store itself + the
    /// PRUNE_DIRS. Returns the entry; the caller pushes it onto the undo
    /// stack. Probes the copy-on-write policy first (a decline surfaces as
    /// an error so the caller skips the snapshot rather than paying a slow
    /// full copy). Callers that have already probed (and got Ok) call
    /// snapshot_after_probe to avoid walking the workspace twice.
    pub fn snapshot(&self, additional: &[PathBuf]) -> std::io::Result<UndoEntry> {
        self.check_reflink_policy()?;
        self.snapshot_after_probe(additional)
    }

    /// The snapshot walk, assuming the copy-on-write policy already passed.
    /// Split out so a caller that probed once and got Ok does not probe
    /// again inside snapshot() -- the probe walks the whole workspace for
    /// its size check, so a second probe doubles the per-command cost on the
    /// destructive-exec hot path. It also closes a small race: a re-probe
    /// after the workspace crosses the threshold would land a decline here
    /// as if it were a real I/O failure.
    pub fn snapshot_after_probe(&self, additional: &[PathBuf]) -> std::io::Result<UndoEntry> {
        let id = self.counter.fetch_add(1, Ordering::Relaxed);
        let snap_dir = self.store_root.join(format!("snap-{id}"));
        // On a mid-walk failure, remove the partial snapshot so a transient
        // I/O error does not leave an orphan dir (prune would reap it
        // eventually, but only after it ages past TTL or trips the size cap).
        if let Err(e) = self.snapshot_walk(&snap_dir, additional) {
            let _cleanup = std::fs::remove_dir_all(&snap_dir);
            return Err(e);
        }
        let entry = UndoEntry::CoWSnapshot {
            store_path: snap_dir,
            extra_roots: additional.to_vec(),
        };
        if let Some(a) = &self.audit {
            a.snapshot_created(&entry);
        }
        Ok(entry)
    }

    fn snapshot_walk(&self, snap_dir: &Path, additional: &[PathBuf]) -> std::io::Result<()> {
        std::fs::create_dir_all(snap_dir)?;
        // The workspace root maps to snap_dir/<rel>. Each additional writable
        // root (user-added working dir, outside the workspace) maps to
        // snap_dir/__extra/<i>/<rel> so restore can route it back to its
        // original root via the extra_roots vec stored on the entry.
        self.walk_root_into(snap_dir, &self.workspace_root)?;
        for (i, root) in additional.iter().enumerate() {
            let extra_dir = snap_dir.join("__extra").join(i.to_string());
            std::fs::create_dir_all(&extra_dir)?;
            self.walk_root_into(&extra_dir, root)?;
        }
        Ok(())
    }

    /// Walk a single root (the workspace or one additional dir) and CoW-copy
    /// its contents into dst preserving the root-relative layout. PRUNE_DIRS
    /// and the snapshot store itself are skipped so a snapshot does not clone
    /// generated trees or prior snapshots.
    fn walk_root_into(&self, dst: &Path, root: &Path) -> std::io::Result<()> {
        for entry in WalkDir::new(root)
            .min_depth(1)
            .into_iter()
            .filter_entry(|e| !is_pruned(e.path(), root, &self.store_root))
        {
            let entry = entry?;
            let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
            let dst_path = dst.join(rel);
            if entry.file_type().is_dir() {
                std::fs::create_dir_all(&dst_path)?;
            } else if entry.file_type().is_file() {
                if let Some(parent) = dst_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                reflink_copy::reflink_or_copy(entry.path(), &dst_path)?;
            }
        }
        Ok(())
    }

    /// Restore the workspace from an undo entry. Full restore from a CoW
    /// snapshot (no 3-way conflict check — a future refinement would refuse
    // if the target was manually changed after the snapshot, with a diff).
    pub fn restore(&self, entry: &UndoEntry) -> std::io::Result<()> {
        let result = self.restore_inner(entry);
        if result.is_ok()
            && let Some(a) = &self.audit
        {
            a.undo_applied(entry);
        }
        result
    }

    fn restore_inner(&self, entry: &UndoEntry) -> std::io::Result<()> {
        match entry {
            UndoEntry::CoWSnapshot {
                store_path,
                extra_roots,
            } => {
                // Restore the workspace tree: entries directly under store_path
                // map back to workspace_root. The __extra/<i>/ subtree maps to
                // extra_roots[i] (the additional writable roots snapshotted
                // alongside). Skip the __extra subtree in the workspace pass.
                self.restore_root_from(store_path, &self.workspace_root, true)?;
                for (i, root) in extra_roots.iter().enumerate() {
                    let extra_dir = store_path.join("__extra").join(i.to_string());
                    if extra_dir.exists() {
                        self.restore_root_from(&extra_dir, root, false)?;
                    }
                }
                Ok(())
            }
            UndoEntry::BeforeImage { path, before } => match before {
                Some(bytes) => std::fs::write(path, bytes),
                None => std::fs::remove_file(path).or_else(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        Ok(())
                    } else {
                        Err(e)
                    }
                }),
            },
        }
    }

    /// Restore a single root (workspace or one additional dir) from its
    /// snapshot sub-tree. When skip_extra is true, the __extra sub-tree is
    /// skipped (it belongs to additional roots, restored in a separate pass).
    fn restore_root_from(
        &self,
        snap_src: &Path,
        dest_root: &Path,
        skip_extra: bool,
    ) -> std::io::Result<()> {
        for entry in WalkDir::new(snap_src).min_depth(1).into_iter() {
            let entry = entry?;
            let rel = entry.path().strip_prefix(snap_src).unwrap_or(entry.path());
            // Skip the __extra subtree in the workspace-restore pass.
            if skip_extra
                && rel
                    .components()
                    .next()
                    .is_some_and(|c| c.as_os_str() == "__extra")
            {
                continue;
            }
            let dst = dest_root.join(rel);
            if entry.file_type().is_dir() {
                std::fs::create_dir_all(&dst)?;
            } else if entry.file_type().is_file() {
                if let Some(parent) = dst.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(entry.path(), &dst)?;
            }
        }
        Ok(())
    }

    /// Prune expired snapshots + enforce a size cap. Removes snapshots older
    /// than the TTL, then if the total store size exceeds the cap, removes
    /// the oldest until under. Snapshots whose paths appear in protected
    /// (entries still referenced by the undo stack) are never removed.
    pub fn prune(&self, ttl_secs: u64, max_size: u64, protected: &[PathBuf]) -> usize {
        let mut removed = 0;
        let now = std::time::SystemTime::now();
        let mut snaps: Vec<(PathBuf, std::time::SystemTime, u64)> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&self.store_root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                // Never prune a snapshot the undo stack still references.
                if protected
                    .iter()
                    .any(|p| path.starts_with(p) || p.starts_with(&path))
                {
                    continue;
                }
                let mtime = entry.metadata().and_then(|m| m.modified()).unwrap_or(now);
                let age = now.duration_since(mtime).unwrap_or_default();
                let size = dir_size(&path);
                if age.as_secs() > ttl_secs {
                    let _r3 = std::fs::remove_dir_all(&path);
                    removed += 1;
                } else {
                    snaps.push((path, mtime, size));
                }
            }
        }
        snaps.sort_by_key(|(_, mtime, _)| *mtime);
        let mut total: u64 = snaps.iter().map(|(_, _, s)| s).sum();
        for (path, _, size) in &snaps {
            if total <= max_size {
                break;
            }
            let _r4 = std::fs::remove_dir_all(path);
            total -= size;
            removed += 1;
        }
        if let Some(a) = &self.audit {
            a.snapshot_pruned(removed);
        }
        removed
    }
}

fn dir_size(path: &Path) -> u64 {
    let mut total: u64 = 0;
    for entry in WalkDir::new(path).min_depth(1).into_iter().flatten() {
        if entry.file_type().is_file() {
            total += entry.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    total
}

/// Pure ext4 fallback policy: when CoW is unavailable, degrade to Ask only if
/// the workspace exceeds the threshold; small workspaces still get a full-copy
/// fallback. Extracted from check_reflink_policy so the ext4 error branch is
/// testable without an actual no-reflink filesystem (can_reflink is the seam).
fn reflink_policy(can_reflink: bool, workspace_size: u64) -> std::io::Result<()> {
    if can_reflink {
        return Ok(());
    }
    if workspace_size > NO_REFLINK_SIZE_THRESHOLD {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!(
                "reflink unavailable (ext4?) + workspace {} bytes > {} threshold; \
                 degrade to Ask instead of slow copy",
                workspace_size, NO_REFLINK_SIZE_THRESHOLD
            ),
        ));
    }
    Ok(())
}

/// Scan the store directory for the highest snap-N id so a fresh process
/// starts its counter past existing snapshots (avoids collision).
fn scan_max_snapshot_id(store_root: &Path) -> u64 {
    let mut max = 0;
    if let Ok(entries) = std::fs::read_dir(store_root) {
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
                && let Some(name) = entry.file_name().to_str()
                && let Some(num) = name.strip_prefix("snap-")
                && let Ok(n) = num.parse::<u64>()
            {
                max = max.max(n);
            }
        }
    }
    max
}

/// True if the path is a pruned directory: the snapshot store itself (under
/// <workspace>/the snapshot store) or one of the PRUNE_DIRS at the
/// workspace root's top level.
fn is_pruned(path: &Path, workspace_root: &Path, store_root: &Path) -> bool {
    if path.starts_with(store_root) {
        return true;
    }
    let rel = path.strip_prefix(workspace_root).unwrap_or(path);
    if let Some(first) = rel.iter().next()
        && PRUNE_DIRS.contains(&first.to_string_lossy().as_ref())
    {
        return true;
    }
    false
}

#[cfg(test)]
#[path = "snapshot_tests.rs"]
mod tests;
