//! Background session housekeeping: the prune that bounds the sessions
//! store, followed by the block reclaim that frees what no session names.
//! A long-lived entry (Serve, Acp, Tui, Resume, Continue) calls
//! start_background_housekeeping after assembling its bundle; the prune
//! fires once after a 10-minute delay, on a spawn_blocking thread so the
//! blocking FS IO (readdir + delete, minutes on a 50k backlog) does not
//! starve the tokio runtime or freeze the TUI. Attach does not call it -
//! it connects to an existing daemon that runs its own.
//!
//! A non-blocking lock on .prune.lock serializes across processes (multi
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

use houyicoder_config::{config_home, retention};
use houyicoder_context::SessionId;
use houyicoder_context::session_class::LOCK_FILE;
use houyicoder_service::lifecycle::prune::{
    self, PruneKind, PrunePlan, PrunePolicy, PruneReport, PruneTargets,
};
use houyicoder_service::lifecycle::reclaim::{ReclaimOutcome, ReclaimPolicy, reclaim_store_blocks};
use houyicoder_service::lifecycle::repair::repair_child_descriptors;

const DELAY_SECS: u64 = 10 * 60;
const MARKER_THROTTLE_SECS: u64 = 24 * 60 * 60;
// EMPTY_TTL_SECS lives in session_prune (the prune engine) so the startup
// backlog notice + the sweep share one default; the snapshot + debug-log
// ceilings below are sweep/cleanup-only and stay here.
const SNAPSHOT_TTL_SECS: u64 = 7 * 24 * 60 * 60;
const DEBUG_MAX_BYTES: u64 = 10 * 1024 * 1024;
use crate::session::lock::FileLock;
#[cfg(test)]
use crate::session::lock::SessionLock;
use crate::session::lock::open_lock_file;
use crate::session::lock::probe_session_lock;
use houyicoder_service::composition::session_log_root;
use houyicoder_service::lifecycle::prune::EMPTY_TTL_SECS;
use std::env::current_dir;
#[cfg(test)]
use std::env::temp_dir;
use std::fs::TryLockError;
use std::fs::create_dir_all;
use std::fs::metadata;
#[cfg(test)]
use std::fs::read;
use std::fs::read_dir;
#[cfg(test)]
use std::fs::remove_dir_all;
use std::fs::write;
#[cfg(test)]
use std::process::id;
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::task::spawn_blocking;
use tokio::time::sleep;

/// Fire-and-forget: after a 10-minute delay the prune runs on a
/// spawn_blocking thread. Call from a long-lived entry's runtime context
/// (inside block_on). The task dies with the process; a session shorter
/// than 10 minutes simply does not prune this run.
pub fn start_background_housekeeping(
    sessions_root: PathBuf,
    debug_log: Option<PathBuf>,
    current_session: SessionId,
) {
    let home = config_home();
    let shell_snapshots = home.join("shell-snapshots");
    tokio::spawn(async move {
        sleep(Duration::from_secs(DELAY_SECS)).await;
        spawn_blocking(move || {
            run_once(
                &home,
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

/// One sweep over one store. The config home is a parameter, not a lookup:
/// the lock, the marker, and the retention settings all live under it, and a
/// caller that names the home names all three.
fn run_once(
    home: &Path,
    sessions_root: &Path,
    shell_snapshots: &Path,
    debug_log: Option<&Path>,
    current_session: &SessionId,
) {
    let _lock = match acquire_prune_lock(home) {
        Ok(lock) => lock,
        Err(PruneLockError::Contended) => {
            tracing::debug!("housekeeping: .prune.lock held by another process, skipping");
            return;
        }
        Err(PruneLockError::Failed(e)) => {
            tracing::warn!("housekeeping: could not take the prune lock: {e}");
            return;
        }
    };

    let marker_path = home.join(".prune-marker");
    if marker_throttled(&marker_path) {
        tracing::debug!("housekeeping: marker < 24h, skipping");
        return;
    }

    // Before the plan: a child whose descriptor never landed is classed right
    // only by reading its log, and every other reader of the store expects
    // the descriptor there. It runs inside the sweep's own lock, so two
    // sweeps over one store are serialized -- unless a store root is
    // redirected away from the config home that holds that lock.
    let repaired = repair_child_descriptors(sessions_root);
    if repaired.written > 0 || repaired.failed > 0 {
        tracing::info!(
            "housekeeping: repaired {} child descriptors, {} failed",
            repaired.written,
            repaired.failed
        );
    }

    let (policy, targets, threshold_raw) = build_prune_context(
        sessions_root,
        shell_snapshots,
        debug_log,
        Some(*current_session),
        home,
    );

    let plan = prune::plan_all(&targets, &policy);
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
        // Skipped sessions still name their blocks, so the reclaim keeps
        // what they reference and collects only what nothing does.
        reclaim_blocks(sessions_root);
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
    // After the prune, so a deleted session's unshared blocks are collected
    // in the same window rather than waiting for the next one.
    reclaim_blocks(sessions_root);
}

/// Reclaim the content-addressed blocks no session log or checkpoint names.
///
/// The pass keeps a block a writer stored but has not yet referenced (the
/// write lease), keeps everything a source it cannot finish reading might
/// name (a scan error stops it), and spends at most its own budget. None of
/// those outcomes deletes a block that a reference names.
fn reclaim_blocks(sessions_root: &Path) {
    match reclaim_store_blocks(sessions_root, &ReclaimPolicy::default()) {
        ReclaimOutcome::Completed(report) | ReclaimOutcome::Partial(report) => tracing::info!(
            "housekeeping: blocks {} stored, {} reclaimed, {} leased, {} deferred, {} sources read, {} refs skipped, {} torn tails, complete {}",
            report.blocks,
            report.removed,
            report.leased,
            report.deferred,
            report.sources,
            report.skipped_refs,
            report.torn_tail,
            report.complete
        ),
        ReclaimOutcome::Skipped => {
            tracing::debug!("housekeeping: block reclaim lock held, skipping");
        }
        ReclaimOutcome::Stopped { error, report } => {
            // The store keeps what the pass could not account for; a sweep
            // that had already finished shards keeps their freed blocks too,
            // so the log line carries that count.
            tracing::warn!(
                "housekeeping: block reclaim stopped after {} freed: {error}",
                report.removed
            );
        }
    }
}

/// Why a prune lock was not taken.
#[derive(Debug)]
pub(crate) enum PruneLockError {
    /// Another process holds the lock; this pass skips rather than queues.
    Contended,
    /// The lock file could not be created, opened, or locked.
    Failed(String),
}

/// Non-blocking exclusive lock on .prune.lock. The guard releases the lock
/// when dropped.
fn acquire_prune_lock(home: &Path) -> Result<FileLock, PruneLockError> {
    let path = home.join(".prune.lock");
    if let Some(parent) = path.parent() {
        let _r = create_dir_all(parent);
    }
    let file = open_lock_file(&path, true)
        .map_err(|e| PruneLockError::Failed(format!("open {path:?}: {e}")))?;
    FileLock::try_acquire(file).map_err(|e| match e {
        TryLockError::WouldBlock => PruneLockError::Contended,
        TryLockError::Error(e) => PruneLockError::Failed(format!("lock {path:?}: {e}")),
    })
}

/// True if the marker was written less than 24h ago (throttle).
fn marker_throttled(path: &Path) -> bool {
    let Ok(meta) = metadata(path) else {
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
        let _r = create_dir_all(parent);
    }
    if write(path, body.to_string()).is_err() {
        tracing::warn!("housekeeping: could not write marker {path:?}");
    }
}

/// Convenience: fire housekeeping with the sessions root + the debug log
/// path derived from the cwd. Call from a long-lived entry's runtime
/// context (inside block_on or a runtime spawn) after the bundle is ready.
pub fn fire_after_bundle(session: SessionId) {
    let root = session_log_root();
    let debug = current_dir()
        .unwrap_or_default()
        .join(".houyicoder")
        .join("debug.log");
    start_background_housekeeping(root, Some(debug), session);
}

/// Scan every session dir for a session.lock held by another live process.
/// The probe is a non-blocking try-lock on each lock file: success means no
/// holder (the probe drops immediately), failure means a live process holds
/// it. mtime recency does not cover a held-but-idle session, so the probe is
/// the real guard, not a fallback.
///
/// This pre-scan has one job that the apply-time filter_live_sessions does
/// NOT cover: it feeds the held sessions into PrunePolicy.protected, so
/// plan_prune excludes them from the count CAP. Without it a held session
/// counts toward max_count, and the cap over-prunes the rest to compensate.
/// The apply-time filter then re-checks each entry right before deletion
/// (a session resumed between this scan and the apply is caught there).
pub(crate) fn scan_lock_held_sessions(sessions_root: &Path) -> Vec<SessionId> {
    let Ok(entries) = read_dir(sessions_root) else {
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
        let lock_path = path.join(LOCK_FILE);
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
fn acquire_delete_lock(session_dir: &Path) -> Option<FileLock> {
    let file = open_lock_file(&session_dir.join(LOCK_FILE), true).ok()?;
    FileLock::try_acquire(file).ok()
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
    home: &Path,
) -> (PrunePolicy, PruneTargets, u32) {
    let (cfg, warnings) = retention::load_retention_in(home);
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
/// guard, held until the caller drops it so apply_prune runs under the lock.
/// Dry-run planning does not need the lock. This is a thin pub(crate) wrapper
/// around the private acquire_prune_lock so the cleanup subcommand does not
/// name the lock file directly.
pub(crate) fn try_prune_lock(home: &Path) -> Result<FileLock, PruneLockError> {
    acquire_prune_lock(home)
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
        let r = prune::apply_prune(&one);
        report.removed += r.removed;
        report.truncated += r.truncated;
        report.errors += r.errors;
    }
    (report, skipped_live)
}

#[cfg(test)]
mod tests;
