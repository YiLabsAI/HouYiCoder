//! Recoverable destructive operations: workspace copy-on-write snapshots
//! and an undo stack. A destructive bash command snapshots the whole
//! workspace tree (reflink per file, O(file count)) before executing; a
//! restore puts it back. File-tool per-file before-images (Write/Edit/
//! MultiEdit) share the same undo stack (the per-file before-image design).
//!
//! The store's default location is workspace-local, because clonefile/FICLONE
//! require one volume and a store on another volume would turn every snapshot
//! into a full copy. The composition root may place it outside the workspace
//! (the caller picks the root through new_at) so the agent's own find and grep
//! do not walk cloned trees; it falls back to the workspace-local root when the
//! two paths are on different volumes. The store directory itself is pruned
//! from the walkdir so a snapshot does not clone prior snapshots
//! (self-referential explosion).

use std::cmp::Reverse;
use std::path::{Path, PathBuf};
use std::process::id;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::agent::git_safety::command_triggers_git_snapshot;
use crate::agent::runner_config::DEFAULT_SNAPSHOT_TTL_SECS;
use std::fs::copy;
use std::fs::create_dir_all;
use std::fs::read_dir;
use std::fs::remove_file;
use std::fs::write;
use std::io::Error;
use std::io::ErrorKind;
use std::time::SystemTime;
use walkdir::WalkDir;

use self::lease::LeaseProbe;
pub use self::lease::SnapshotLease;
use self::lease::StoreLock;
use self::lease::StoreProbe;
use self::lease::claim;
use self::lease::mark_legacy_sentinel;
use self::lease::sentinel_path;
use self::lease::try_store_lock;
use self::migrate::migrate_store;
use self::migrate::scan_max_snapshot_id;
use self::prune::enforce_retention;
use self::staging::is_complete;
use self::staging::pause_at_build_barrier;
use self::staging::publish;
use self::staging::start_build;

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
    // the tree before they run so the undo stack can recover. Argument-aware so a bare
    // git checkout <branch> switch does NOT trigger a snapshot. Matches
    // the consent-gate classifier in the permission crate; keep both in
    // sync.
    let has_git_discard = command_triggers_git_snapshot(command);
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
        /// The store lease that keeps prune away from this snapshot for as
        /// long as the entry is alive.
        lease: SnapshotLease,
    },
    BeforeImage {
        path: PathBuf,
        before: Option<Vec<u8>>,
    },
}

impl UndoEntry {
    /// A human-readable description of what was undone, for the restore reply.
    /// Does not oversell: says "restored N files from snapshot" (honest about
    /// the full-restore semantics — new files created after the snapshot
    /// are NOT deleted, .git is pruned from snapshots, symlinks are skipped).
    /// A snapshot without the complete marker is usually older than the
    /// marker, and a marker lost after publication cannot be told apart from
    /// that, so the reply says the tree is unverified rather than claiming a
    /// whole copy.
    pub fn description(&self) -> String {
        match self {
            UndoEntry::CoWSnapshot { store_path, .. } => {
                let count = count_restored_files(store_path);
                if is_complete(store_path) {
                    format!("restored {count} files from snapshot")
                } else {
                    format!("restored {count} files from an unverified snapshot")
                }
            }
            UndoEntry::BeforeImage { .. } => "restored file from before-image".into(),
        }
    }
}

/// The outcome of a restore attempt. Distinguishes empty stack, success, and
/// restore failure — the caller surfaces each to the user so a restore
/// failure is never confused with an empty stack.
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

/// The undo stack: strict LIFO. A restore pops the most recent entry.
#[derive(Debug, Default)]
pub struct UndoStack {
    entries: Vec<UndoEntry>,
}

impl UndoStack {
    pub fn new() -> Self {
        Self::default()
    }

    /// Keep at most MAX_UNDO_DEPTH snapshot entries. Each one holds an open
    /// lease file for as long as it is alive, so an unbounded stack would
    /// exhaust the process file table over a long session. The oldest
    /// snapshot entry is dropped instead, which releases its lease and lets
    /// prune reclaim the tree. Before-image entries hold no handle and are
    /// not counted.
    pub fn push(&mut self, entry: UndoEntry) {
        if matches!(entry, UndoEntry::CoWSnapshot { .. }) {
            let snapshots = self
                .entries
                .iter()
                .filter(|entry| matches!(entry, UndoEntry::CoWSnapshot { .. }))
                .count();
            if snapshots >= MAX_UNDO_DEPTH
                && let Some(oldest) = self
                    .entries
                    .iter()
                    .position(|entry| matches!(entry, UndoEntry::CoWSnapshot { .. }))
            {
                let _evicted = self.entries.remove(oldest);
            }
        }
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
    /// processes; only the in-memory stack is lost. Entries must be in push
    /// order, oldest first, because pop takes the last one: the newest
    /// snapshot has to end up at the end of the vec.
    pub fn from_entries(entries: Vec<UndoEntry>) -> Self {
        Self { entries }
    }
}

/// The most snapshots the undo stack holds. It bounds the window a resume
/// re-links, so older snapshots stay for prune to reclaim, and it caps the
/// live stack, because each snapshot entry keeps one lease file open for as
/// long as it is alive.
const MAX_UNDO_DEPTH: usize = 10;

/// The default threshold (bytes) above which a full-copy fallback on a
/// no-reflink filesystem degrades to Ask instead of paying a slow copy per
/// destructive command.
const NO_REFLINK_SIZE_THRESHOLD: u64 = 512 * 1024 * 1024;

/// Distinguishes the probe files of concurrent probes in one process. Two
/// processes already differ by process id; this separates two stores opened
/// in one process, which would otherwise share one probe path.
static PROBE_SEQ: AtomicU64 = AtomicU64::new(0);

/// The snapshot store. Snapshots are whole-tree CoW clones
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
    /// Open the workspace-local store. The store lives beside the workspace
    /// because clonefile and FICLONE only work within one volume.
    pub fn new(workspace_root: impl AsRef<Path>) -> std::io::Result<Self> {
        let workspace_root = workspace_root.as_ref().to_path_buf();
        let store_root = workspace_root.join(".houyicoder").join("snapshots");
        Self::new_at(workspace_root, store_root)
    }

    /// Open a store at an explicit root. The caller picks the location: the
    /// composition root keeps it outside the workspace so the agent's own find
    /// and grep do not walk cloned trees, and falls back to the workspace-local
    /// default when the two paths are on different volumes.
    pub fn new_at(
        workspace_root: impl AsRef<Path>,
        store_root: impl AsRef<Path>,
    ) -> std::io::Result<Self> {
        let workspace_root = workspace_root.as_ref().to_path_buf();
        let store_root = store_root.as_ref().to_path_buf();
        // A store that moved out of the workspace still owes the undo history
        // it holds: the previous location's snapshots move in before the
        // counter is scanned, so their ids keep their meaning. Both namespace
        // locks are claimed without waiting, so two paths naming one
        // directory count as busy rather than deadlocking.
        let legacy = workspace_root.join(".houyicoder").join("snapshots");
        if legacy != store_root && legacy.is_dir() {
            match try_store_lock(&legacy) {
                StoreProbe::Claimed(legacy_lock) => match try_store_lock(&store_root) {
                    StoreProbe::Claimed(destination_lock) => {
                        migrate_store(&legacy, &store_root, legacy_lock, destination_lock);
                    }
                    StoreProbe::Busy => tracing::warn!(
                        "snapshot store move skipped: {} is in use by another process",
                        store_root.display()
                    ),
                    StoreProbe::Unreadable => tracing::warn!(
                        "snapshot store move skipped: {} cannot be read",
                        store_root.display()
                    ),
                },
                StoreProbe::Busy => tracing::warn!(
                    "snapshot store move skipped: {} is in use by another process",
                    legacy.display()
                ),
                StoreProbe::Unreadable => tracing::warn!(
                    "snapshot store move skipped: {} cannot be read",
                    legacy.display()
                ),
            }
        }
        // Scan existing snap-N dirs to avoid id collision across restarts.
        // The previous location counts too, so a snapshot the move left
        // behind keeps its name reserved and a new build never takes an id
        // the next start would skip.
        let legacy_max = if legacy.is_dir() {
            scan_max_snapshot_id(&legacy)
        } else {
            0
        };
        let start = scan_max_snapshot_id(&store_root)
            .max(legacy_max)
            .saturating_add(1);
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
    /// resumed session can undo a destructive operation from the prior
    /// process. The snapshots persist on disk across restarts; only the
    /// in-memory stack is lost. The result is in LIFO order: the last element
    /// is the newest snapshot, which is the one a pop must hand back first.
    ///
    /// Only snapshots a resume can still act on are re-linked. One older than
    /// the retention TTL is left for prune, and the stack stops at the undo
    /// depth. A re-link that loaded every directory on disk would mark all of
    /// them protected, and prune would then never reclaim any of them.
    ///
    /// Workspace-only snapshots re-link cleanly (restore maps the cloned tree
    /// back to the workspace root). Snapshots that carried additional writable
    /// roots (an __extra subtree) are skipped: the original extra_roots
    /// mapping is not on disk, so a restore could not remap the extra
    /// subtrees back to their roots. Restoring those needs that mapping on disk
    /// (a future refinement); today they are left on disk, unrecoverable from
    /// the undo stack after a restart, the same as before this re-link.
    pub fn relink_undo_entries(&self) -> Vec<UndoEntry> {
        self.relink_undo_entries_within(DEFAULT_SNAPSHOT_TTL_SECS)
    }

    /// Re-link with an explicit retention. The store keeps whatever TTL its
    /// owner configured; reading a snapshot back that prune would already
    /// consider expired would put the two retention decisions on one dataset
    /// out of step.
    pub fn relink_undo_entries_within(&self, ttl_secs: u64) -> Vec<UndoEntry> {
        // A store that was never created holds nothing to re-link, and a
        // session that never takes a snapshot leaves no directory behind.
        if !self.store_root.is_dir() {
            return Vec::new();
        }
        let Ok(_lock) = StoreLock::acquire(&self.store_root) else {
            tracing::warn!("snapshot store lock unavailable; re-link skipped");
            return Vec::new();
        };
        let now = SystemTime::now();
        let mut candidates: Vec<(u64, String, PathBuf)> = Vec::new();
        let dir = match read_dir(&self.store_root) {
            Ok(dir) => dir,
            Err(error) => {
                tracing::warn!(
                    "snapshot store {} cannot be listed: {error}",
                    self.store_root.display()
                );
                return Vec::new();
            }
        };
        for entry in dir.flatten() {
            let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
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
            let age = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|at| now.duration_since(at).ok())
                .unwrap_or_default();
            if age.as_secs() > ttl_secs {
                continue;
            }
            let snap_path = self.store_root.join(name);
            // Skip additional-roots snapshots: the __extra subtree's
            // root mapping is not on disk, so restore could not remap.
            if snap_path.join("__extra").exists() {
                continue;
            }
            candidates.push((n, name.to_owned(), snap_path));
        }
        // Newest first: claiming stops at the undo depth, so a store holding
        // many snapshots never opens more leases than the stack can use.
        candidates.sort_by_key(|(n, _, _)| Reverse(*n));
        let mut entries: Vec<UndoEntry> = Vec::new();
        for (_, name, snap_path) in candidates {
            if entries.len() >= MAX_UNDO_DEPTH {
                break;
            }
            // The claim keeps this process from sharing a snapshot another
            // live process holds; a busy one is left for that process.
            let sentinel = sentinel_path(&self.store_root, &name);
            let complete = is_complete(&snap_path);
            // A tree with neither a marker nor a sentinel predates both. The
            // tag is written before the claim, so a death after the claim
            // never leaves an empty sentinel beside a markerless tree, which
            // the next start reads as a failed publish.
            if !complete
                && !sentinel.exists()
                && let Err(error) = mark_legacy_sentinel(&sentinel)
            {
                tracing::warn!(
                    "snapshot {} cannot be tagged as a pre-marker snapshot: {error}",
                    snap_path.display()
                );
                continue;
            }
            let LeaseProbe::Claimed(lease) = claim(&sentinel) else {
                continue;
            };
            // An empty sentinel beside a markerless tree was written by a
            // publish that never finished. Its tree may be half written, so
            // the undo stack must not offer it.
            if !complete {
                if !lease.carries_legacy_tag() {
                    tracing::warn!(
                        "snapshot {} has a lease but no complete marker; left on disk",
                        snap_path.display()
                    );
                    continue;
                }
                tracing::warn!(
                    "snapshot {} predates the complete marker; re-linked unverified",
                    snap_path.display()
                );
            }
            entries.push(UndoEntry::CoWSnapshot {
                store_path: snap_path,
                extra_roots: Vec::new(),
                lease,
            });
        }
        entries.reverse();
        entries
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
            if create_dir_all(&self.store_root).is_err() {
                return false;
            }
            // A probe owns its two file names: a shared name would let a
            // concurrent probe consume the source or find the destination
            // taken, and report a reflink filesystem as copy-only.
            let probe = format!(
                ".reflink-test-{}-{}",
                id(),
                PROBE_SEQ.fetch_add(1, Ordering::Relaxed)
            );
            let src = self.store_root.join(format!("{probe}-src"));
            let dst = self.store_root.join(format!("{probe}-dst"));
            if write(&src, b"t").is_err() {
                return false;
            }
            let ok = reflink_copy::reflink(&src, &dst).is_ok();
            let _r1 = remove_file(&src);
            let _r2 = remove_file(&dst);
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
        // The build fills a private staging tree under a lease, so no other
        // process can observe it as a snapshot until it is published whole.
        let staged = start_build(&self.store_root)?;
        pause_at_build_barrier("staged");
        if let Err(error) = self.snapshot_walk(staged.path(), additional) {
            staged.discard();
            return Err(error);
        }
        pause_at_build_barrier("copied");
        let (store_path, lease) = publish(staged, &self.counter, &self.store_root)?;
        let entry = UndoEntry::CoWSnapshot {
            store_path,
            extra_roots: additional.to_vec(),
            lease,
        };
        if let Some(a) = &self.audit {
            a.snapshot_created(&entry);
        }
        Ok(entry)
    }

    fn snapshot_walk(&self, snap_dir: &Path, additional: &[PathBuf]) -> std::io::Result<()> {
        // The workspace root maps to snap_dir/<rel>. Each additional writable
        // root (user-added working dir, outside the workspace) maps to
        // snap_dir/__extra/<i>/<rel> so restore can route it back to its
        // original root via the extra_roots vec stored on the entry.
        self.walk_root_into(snap_dir, &self.workspace_root, true)?;
        for (i, root) in additional.iter().enumerate() {
            let extra_dir = snap_dir.join("__extra").join(i.to_string());
            create_dir_all(&extra_dir)?;
            self.walk_root_into(&extra_dir, root, false)?;
        }
        Ok(())
    }

    /// Walk a single root (the workspace or one additional dir) and CoW-copy
    /// its contents into dst preserving the root-relative layout. PRUNE_DIRS
    /// and the snapshot store itself are skipped so a snapshot does not clone
    /// generated trees or prior snapshots. The workspace pass stops once
    /// after its first copied file when a test build barrier is armed.
    fn walk_root_into(&self, dst: &Path, root: &Path, first_root: bool) -> std::io::Result<()> {
        let mut announced = !first_root;
        for entry in WalkDir::new(root)
            .min_depth(1)
            .into_iter()
            .filter_entry(|e| !is_pruned(e.path(), root, &self.store_root))
        {
            let entry = entry?;
            let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
            let dst_path = dst.join(rel);
            if entry.file_type().is_dir() {
                create_dir_all(&dst_path)?;
            } else if entry.file_type().is_file() {
                if let Some(parent) = dst_path.parent() {
                    create_dir_all(parent)?;
                }
                reflink_copy::reflink_or_copy(entry.path(), &dst_path)?;
                if !announced {
                    announced = true;
                    pause_at_build_barrier("first-file");
                }
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
                ..
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
                Some(bytes) => write(path, bytes),
                None => remove_file(path).or_else(|e| {
                    if e.kind() == ErrorKind::NotFound {
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
            // Skip the __extra subtree in the workspace-restore pass: it belongs
            // to additional roots and is restored separately.
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
                create_dir_all(&dst)?;
            } else if entry.file_type().is_file() {
                if let Some(parent) = dst.parent() {
                    create_dir_all(parent)?;
                }
                copy(entry.path(), &dst)?;
            }
        }
        Ok(())
    }

    /// Prune expired snapshots + enforce a size cap. Removes snapshots older
    /// than the TTL, then if the total store size exceeds the cap, removes
    /// the oldest until under. Snapshots whose paths appear in protected
    /// (entries still referenced by the undo stack) are never removed, and a
    /// snapshot another process holds a lease on is always kept.
    pub fn prune(&self, ttl_secs: u64, max_size: u64, protected: &[PathBuf]) -> usize {
        let removed = enforce_retention(&self.store_root, ttl_secs, max_size, protected);
        if let Some(a) = &self.audit {
            a.snapshot_pruned(removed);
        }
        removed
    }
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
        return Err(Error::new(
            ErrorKind::Unsupported,
            format!(
                "reflink unavailable (ext4?) + workspace {} bytes > {} threshold; \
                 degrade to Ask instead of slow copy",
                workspace_size, NO_REFLINK_SIZE_THRESHOLD
            ),
        ));
    }
    Ok(())
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

mod lease;
mod migrate;
mod prune;
mod staging;

pub mod file_transaction;

#[cfg(test)]
mod tests;
