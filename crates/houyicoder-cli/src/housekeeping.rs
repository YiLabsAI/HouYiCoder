//! Background session housekeeping: the prune that bounds the sessions
//! store. A long-lived entry (Serve, Acp, Tui, Resume, Continue) calls
//! start_background_housekeeping after assembling its bundle; the prune
//! fires once after a 10-minute delay, on a spawn_blocking thread so the
//! blocking FS IO (readdir + delete, minutes on a 50k backlog) does not
//! starve the tokio runtime or freeze the TUI. Attach does not call it -
//! it connects to an existing daemon that runs its own.
//!
//! A non-blocking flock on .prune.lock serializes across processes (multi
//! worktree is the norm; a second holder skips, not queues). A marker file
//! stores the last PruneReport as JSON and throttles to once per 24h; the
//! marker is the evidence trail for an irreversible deletion, not just a
//! touched mtime. The routing threshold (from settings) decides whether the
//! sweep applies (below threshold) or skips auto-apply (at or above) -- a
//! plan too large to delete blindly is left for the manual cleanup
//! subcommand, so the auto path's run time is bounded, not assumed. The
//! spawn is single-fire per launch (no periodic cycle); a skipped sweep
//! re-evaluates on the next long-lived launch, or via the houyi cleanup
//! subcommand, which is the reachable review path. The skip message is
//! tracing-only -- the housekeeping task has no TUI handle (and the TUI
//! may have exited before this 10-min fire), so routing it to the system
//! line is a separate wiring task.

use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(unix)]
use nix::fcntl::{Flock, FlockArg};
#[cfg(unix)]
use std::fs::OpenOptions;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use houyicoder_config::{config_home, retention};
use houyicoder_context::SessionId;
use houyicoder_service::session_prune::{
    self, PruneKind, PrunePlan, PrunePolicy, PruneReport, PruneTargets,
};

const DELAY_SECS: u64 = 10 * 60;
const MARKER_THROTTLE_SECS: u64 = 24 * 60 * 60;
// EMPTY_TTL_SECS lives in session_prune (the prune engine) so the startup
// backlog notice + the sweep share one default; the snapshot + debug-log
// ceilings below are sweep/cleanup-only and stay here.
const SNAPSHOT_TTL_SECS: u64 = 7 * 24 * 60 * 60;
const DEBUG_MAX_BYTES: u64 = 10 * 1024 * 1024;
use houyicoder_service::session_prune::EMPTY_TTL_SECS;

/// Fire-and-forget: after a 10-minute delay the prune runs on a
/// spawn_blocking thread. Call from a long-lived entry's runtime context
/// (inside block_on). The task dies with the process; a session shorter
/// than 10 minutes simply does not prune this run.
pub fn start_background_housekeeping(
    sessions_root: PathBuf,
    debug_log: Option<PathBuf>,
    current_session: SessionId,
) {
    let shell_snapshots = config_home().join("shell-snapshots");
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(DELAY_SECS)).await;
        tokio::task::spawn_blocking(move || {
            run_once(
                &sessions_root,
                &shell_snapshots,
                debug_log.as_deref(),
                &current_session,
            );
        })
        .await
        .ok();
    });
}

fn run_once(
    sessions_root: &Path,
    shell_snapshots: &Path,
    debug_log: Option<&Path>,
    current_session: &SessionId,
) {
    let _lock = match acquire_prune_lock() {
        Some(l) => l,
        None => {
            tracing::debug!("housekeeping: .prune.lock held by another process, skipping");
            return;
        }
    };

    let marker_path = config_home().join(".prune-marker");
    if marker_throttled(&marker_path) {
        tracing::debug!("housekeeping: marker < 24h, skipping");
        return;
    }

    let (policy, targets, threshold_raw) = build_prune_context(
        sessions_root,
        shell_snapshots,
        debug_log,
        Some(*current_session),
    );

    let plan = session_prune::plan_all(&targets, &policy);
    let threshold = threshold_raw as usize;
    if plan.len() >= threshold {
        // Skip auto-apply: the plan is too large to delete blindly (a 50k
        // backlog auto-delete is catastrophic if the threshold is mis-set).
        // Do NOT write the marker -- the default-marker path burned the 24h
        // throttle without deleting, skipping the next launch for a day.
        // Without the marker, the next long-lived launch re-evaluates (or a
        // houyi cleanup runs in between). Single-fire spawn, no cycle: the
        // reachable review path is the houyi cleanup subcommand. The skip
        // message is tracing-only here -- the task has no TUI handle (the
        // TUI may have exited before this 10-min fire); routing it to the
        // system line is a separate fix.
        tracing::info!(
            "housekeeping: {} entries prunable (>= threshold {}); \
             run `houyi cleanup` to review",
            plan.len(),
            threshold
        );
        return;
    }
    // Delete under the per-session lock: a session may have been resumed
    // between the pre-scan (scan_lock_held_sessions) and now, since the plan
    // took seconds to minutes to build.
    let planned = plan.len();
    let (report, skipped) = apply_prune_locked(&plan);
    tracing::info!(
        "housekeeping: pruned {} entries, truncated {} logs, {} skipped (live), {} errors",
        report.removed,
        report.truncated,
        skipped,
        report.errors
    );
    write_marker(&marker_path, &report, planned);
}

/// Non-blocking exclusive flock on .prune.lock. None if held by another
/// process (skip, do not queue). RAII: the Flock guard is returned + its
/// Drop releases. Unix-only (flock is a unix primitive); a non-unix build
/// has no cross-process lock (best-effort, single-process).
#[cfg(unix)]
fn acquire_prune_lock() -> Option<Flock<std::fs::File>> {
    let path = config_home().join(".prune.lock");
    if let Some(parent) = path.parent() {
        let _r = std::fs::create_dir_all(parent);
    }
    let file = match OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(f) => f,
        Err(_) => return None,
    };
    Flock::lock(file, FlockArg::LockExclusiveNonblock).ok()
}

#[cfg(not(unix))]
fn acquire_prune_lock() -> Option<()> {
    Some(())
}

/// True if the marker was written less than 24h ago (throttle).
fn marker_throttled(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    modified
        .elapsed()
        .map(|e| e.as_secs() < MARKER_THROTTLE_SECS)
        .unwrap_or(false)
}

/// Write the PruneReport + the prunable count as JSON to the marker. The
/// marker is the evidence trail for irreversible deletion: a user asking
/// "where did my sessions go" reads this file. Best-effort (a write
/// failure logs + the prune still happened).
fn write_marker(path: &Path, report: &PruneReport, prunable: usize) {
    let body = serde_json::json!({
        "removed": report.removed,
        "truncated": report.truncated,
        "errors": report.errors,
        "prunable": prunable,
    });
    if let Some(parent) = path.parent() {
        let _r = std::fs::create_dir_all(parent);
    }
    if std::fs::write(path, body.to_string()).is_err() {
        tracing::warn!("housekeeping: could not write marker {path:?}");
    }
}

/// Convenience: fire housekeeping with the sessions root + the debug log
/// path derived from the cwd. Call from a long-lived entry's runtime
/// context (inside block_on or a runtime spawn) after the bundle is ready.
pub fn fire_after_bundle(session: SessionId) {
    let root = houyicoder_service::composition::session_log_root();
    let debug = std::env::current_dir()
        .unwrap_or_default()
        .join(".houyicoder")
        .join("debug.log");
    start_background_housekeeping(root, Some(debug), session);
}

/// Scan every session dir for a session.lock held by another live process.
/// The probe is a non-blocking try-flock on each lock file: success means no
/// holder (the probe drops immediately), failure (EAGAIN) means a live
/// process holds it. mtime recency does not cover a held-but-idle session,
/// so the probe is the real guard, not a fallback. Unix-only (flock);
/// non-unix returns empty (no cross-process lock, single-process only).
///
/// This pre-scan has one job that the apply-time filter_live_sessions does
/// NOT cover: it feeds the held sessions into PrunePolicy.protected, so
/// plan_prune excludes them from the count CAP. Without it a held session
/// counts toward max_count, and the cap over-prunes the rest to compensate.
/// The apply-time filter then re-checks each entry right before deletion
/// (a session resumed between this scan and the apply is caught there).
#[cfg(unix)]
pub(crate) fn scan_lock_held_sessions(sessions_root: &Path) -> Vec<SessionId> {
    use houyicoder_context::SessionId;
    let Ok(entries) = std::fs::read_dir(sessions_root) else {
        return Vec::new();
    };
    let mut held = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(sid_str) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(sid) = SessionId::from_display_string(sid_str) else {
            continue;
        };
        let lock_path = path.join("session.lock");
        if !lock_path.exists() {
            continue;
        }
        // Some = acquired (no holder; the probe drops it). None = held by
        // another process OR unopenable -- both protect (fail-closed).
        if probe_session_lock(&lock_path).is_none() {
            held.push(sid);
        }
    }
    held
}

#[cfg(not(unix))]
pub(crate) fn scan_lock_held_sessions(_sessions_root: &Path) -> Vec<SessionId> {
    Vec::new()
}

/// Probe an existing session.lock without creating it: acquire the flock,
/// and the caller drops the guard immediately. Some means no live process
/// held it. None means held (EAGAIN) or unopenable (permissions, IO); both
/// count as protected, since an ambiguous lock state must never allow
/// deleting the session.
///
/// Deliberately does NOT create a missing lock file, unlike the acquire the
/// delete path uses. This probe walks every directory under the sessions
/// root, so creating on the way would write a lock file into tens of
/// thousands of directories it has no intention of touching. The two are
/// separate functions rather than one with a create flag because folding
/// them together is what hid the missing create from review once already.
#[cfg(unix)]
fn probe_session_lock(lock_path: &Path) -> Option<Flock<std::fs::File>> {
    let file = OpenOptions::new().write(true).open(lock_path).ok()?;
    Flock::lock(file, FlockArg::LockExclusiveNonblock).ok()
}

/// Acquire the session lock for a directory the caller is about to delete,
/// creating the lock file when it is absent. The create is the load-bearing
/// half: a session no live process has opened yet has no session.lock, and a
/// resume creates one on its way in. So an acquire that skipped an absent
/// file would hand the delete an unlocked entry, a resume starting a moment
/// later would create and take its own fresh lock, and the delete would land
/// on a session that is now live. Creating the file here means that resume
/// contends with this guard and fails, which is the whole protection.
///
/// None means the lock is held by a live process, or the file could not be
/// created or opened. All three protect the session (fail-closed).
#[cfg(unix)]
fn acquire_delete_lock(session_dir: &Path) -> Option<Flock<std::fs::File>> {
    // truncate(false) on purpose: the open happens before the flock, so it
    // can land on a file a live session holds, and rewriting another
    // process's lock file is no business of a probe. Nothing reads the
    // content anyway -- only the flock on the inode carries meaning.
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(session_dir.join("session.lock"))
        .ok()?;
    Flock::lock(file, FlockArg::LockExclusiveNonblock).ok()
}

/// Build the prune policy + targets from the retention config + the
/// lock-held scan. Shared by the background sweep (run_once) and the
/// manual cleanup subcommand (run_cleanup) so the two paths cannot drift
/// onto different constants or a different protected set. The current
/// session (Some for the sweep, None for standalone cleanup) is merged
/// into the protected set alongside the lock-held sessions.
pub(crate) fn build_prune_context(
    sessions_root: &Path,
    shell_snapshots: &Path,
    debug_log: Option<&Path>,
    current_session: Option<SessionId>,
) -> (PrunePolicy, PruneTargets, u32) {
    let (cfg, warnings) = retention::load_retention();
    for w in &warnings {
        tracing::warn!("retention config: {}: {}", w.field, w.reason);
    }
    let mut protected = scan_lock_held_sessions(sessions_root);
    if let Some(sid) = current_session {
        protected.push(sid);
    }
    let policy = PrunePolicy {
        ttl_secs: (cfg.session_retention_days as u64) * 24 * 3600,
        empty_ttl_secs: EMPTY_TTL_SECS,
        max_count: cfg.session_retention_count as usize,
        protected,
        snapshot_ttl_secs: SNAPSHOT_TTL_SECS,
        debug_max_bytes: DEBUG_MAX_BYTES,
    };
    let targets = PruneTargets {
        sessions_root: Some(sessions_root.to_path_buf()),
        shell_snapshots_root: Some(shell_snapshots.to_path_buf()),
        debug_log: debug_log.map(|p| p.to_path_buf()),
    };
    (policy, targets, cfg.prune_confirm_threshold)
}

/// Try to acquire the prune lock for a manual cleanup --apply. Returns the
/// guard (held until the caller drops it, so apply_prune runs under the
/// lock) or None if held by another process. Dry-run planning does not
/// need the lock. This is a thin pub(crate) wrapper around the private
/// acquire_prune_lock so main.rs does not name the Flock type directly.
#[cfg(unix)]
pub(crate) fn try_prune_lock() -> Option<Flock<std::fs::File>> {
    acquire_prune_lock()
}

#[cfg(not(unix))]
#[allow(dead_code)]
pub(crate) fn try_prune_lock() -> Option<()> {
    Some(())
}

/// Apply a plan one entry at a time, each session deleted while this process
/// holds that session's lock. Returns the report plus the number of sessions
/// held out because a live process had the lock.
///
/// The lock is taken and released per entry rather than for the whole plan.
/// It only ever needs to cover the deletion of its own session, and the
/// per-plan form does not survive the sizes this prune exists to handle: one
/// held lock is one open descriptor, the manual cleanup path applies an
/// unbounded plan, and the default file-descriptor limit on macOS is 256. A
/// plan of tens of thousands would exhaust descriptors partway through, and
/// because an unopenable lock counts as protected, every entry after that
/// point would be reported as live. The user would read "45750 skipped
/// (live)" and have no way to tell it was a descriptor ceiling.
///
/// Non-session entries (shell snapshots, the debug log) take no lock -- no
/// process holds a lock on them, and the debug log is rotated, not removed.
/// The held-out count is returned rather than dropped so a caller can
/// explain why a plan of N removed fewer than N.
#[cfg(unix)]
pub(crate) fn apply_prune_locked(plan: &PrunePlan) -> (PruneReport, usize) {
    let mut report = PruneReport::default();
    let mut skipped_live = 0;
    for entry in &plan.entries {
        // The guard, when a session entry takes one, lives to the end of
        // this iteration, so it spans the delete below and drops before the
        // next entry. That is the ordering the protection depends on.
        let _guard = if entry.kind == PruneKind::Session && entry.path.is_dir() {
            match acquire_delete_lock(&entry.path) {
                Some(lock) => Some(lock),
                None => {
                    // Held by a live process, or unopenable. Protect it.
                    skipped_live += 1;
                    continue;
                }
            }
        } else {
            // A non-session entry, or a directory that vanished between the
            // plan and now (a concurrent prune, a manual delete). Neither
            // takes a lock; a vanished directory is not a live session, so
            // it must not inflate the live count.
            None
        };
        let one = PrunePlan {
            entries: vec![entry.clone()],
            kept: 0,
        };
        let r = session_prune::apply_prune(&one);
        report.removed += r.removed;
        report.truncated += r.truncated;
        report.errors += r.errors;
    }
    (report, skipped_live)
}

#[cfg(not(unix))]
pub(crate) fn apply_prune_locked(plan: &PrunePlan) -> (PruneReport, usize) {
    (session_prune::apply_prune(plan), 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_service::session_prune::{PruneAction, PruneEntry, PruneReason};
    use std::path::PathBuf;

    fn temp_root() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("housekeeping-{n}-{}", std::process::id()));
        let _r = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// A one-entry plan removing a session directory. with_lock controls
    /// whether the directory already carries a session.lock: a session no
    /// live process has ever opened does not have one, which is the case the
    /// delete-time acquire has to create.
    fn plan_with_session(dir: &Path, with_lock: bool) -> PrunePlan {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("log.jsonl"), b"[]").unwrap();
        if with_lock {
            std::fs::write(dir.join("session.lock"), b"").unwrap();
        }
        PrunePlan {
            entries: vec![PruneEntry {
                path: dir.to_path_buf(),
                kind: PruneKind::Session,
                reason: PruneReason::Ttl,
                last_active: 0,
                action: PruneAction::RemoveDir,
            }],
            kept: 0,
        }
    }

    /// A session nobody holds is deleted and reported as removed. It starts
    /// without a session.lock, so this also covers the acquire creating one
    /// on a directory that has never been opened by a live process.
    #[cfg(unix)]
    #[test]
    fn test_apply_removes_free_session() {
        let root = temp_root();
        let sid_dir = root.join("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
        let plan = plan_with_session(&sid_dir, false);
        let (report, skipped) = apply_prune_locked(&plan);
        assert_eq!(report.removed, 1, "free session removed");
        assert_eq!(skipped, 0, "nothing skipped");
        assert!(!sid_dir.exists(), "the directory is gone");
        let _r = std::fs::remove_dir_all(&root);
    }

    /// A session whose lock a live process holds survives, and the skip is
    /// counted rather than dropped. Fail-closed: protect, do not delete.
    #[cfg(unix)]
    #[test]
    fn test_apply_spares_held_session() {
        let root = temp_root();
        let sid_dir = root.join("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb");
        let plan = plan_with_session(&sid_dir, true);
        // Stand in for a live process holding the session open.
        let _external = Flock::lock(
            OpenOptions::new()
                .write(true)
                .open(sid_dir.join("session.lock"))
                .unwrap(),
            FlockArg::LockExclusiveNonblock,
        )
        .unwrap();
        let (report, skipped) = apply_prune_locked(&plan);
        assert_eq!(report.removed, 0, "a held session is not deleted");
        assert_eq!(skipped, 1, "the skip is counted, not silently dropped");
        assert!(sid_dir.exists(), "the held session survives");
        let _r = std::fs::remove_dir_all(&root);
    }

    /// The property the whole lock exists for: while the prune holds a
    /// session, a resume of that same session cannot start. Driven through
    /// the real SessionLock the resume path uses, not an imitation of it, so
    /// the two sides are proven to contend rather than assumed to.
    ///
    /// The session starts with no session.lock, which is the ordinary state
    /// for an old session in a plan -- the file only appears once a process
    /// has opened the session. An acquire that skipped an absent lock file
    /// left this case entirely unprotected: the resume created its own lock,
    /// succeeded, and the delete landed on a live session.
    #[cfg(unix)]
    #[test]
    fn test_delete_lock_blocks_resume() {
        let root = temp_root();
        let sid_str = "cccccccc-cccc-cccc-cccc-cccccccccccc";
        let sid_dir = root.join(sid_str);
        std::fs::create_dir_all(&sid_dir).unwrap();
        std::fs::write(sid_dir.join("log.jsonl"), b"[]").unwrap();
        assert!(
            !sid_dir.join("session.lock").exists(),
            "the fixture must start with no lock file for this to mean anything"
        );
        let guard = acquire_delete_lock(&sid_dir).expect("prune acquires the delete lock");
        let resumed = crate::session_lock::SessionLock::acquire(sid_str, &root);
        assert!(
            resumed.is_err(),
            "a resume started the session the prune is about to delete"
        );
        drop(guard);
        // Once the prune releases, a resume is free to take the session again.
        assert!(
            crate::session_lock::SessionLock::acquire(sid_str, &root).is_ok(),
            "the lock must not outlive the delete it guards"
        );
        let _r = std::fs::remove_dir_all(&root);
    }
}
