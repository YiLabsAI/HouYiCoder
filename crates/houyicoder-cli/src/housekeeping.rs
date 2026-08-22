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
//! sweep applies silently (below threshold) or reports for manual review
//! (at or above) - the slow first sweep routes to the manual path, so the
//! auto path's run time is bounded, not assumed.

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
use houyicoder_service::session_prune::{self, PrunePolicy, PruneReport, PruneTargets};

const DELAY_SECS: u64 = 10 * 60;
const MARKER_THROTTLE_SECS: u64 = 24 * 60 * 60;
const EMPTY_TTL_SECS: u64 = 24 * 60 * 60;
const SNAPSHOT_TTL_SECS: u64 = 7 * 24 * 60 * 60;
const DEBUG_MAX_BYTES: u64 = 10 * 1024 * 1024;

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

    let (cfg, _warnings) = retention::load_retention();
    // Probe session.lock on every session dir: a lock held by another live
    // process means that session is in use, and pruning it deletes the
    // directory from under the writer. mtime recency does not cover a held
    // but idle session (its log is old), so the probe is the real guard.
    let mut protected = scan_lock_held_sessions(sessions_root);
    protected.push(*current_session);
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

    let plan = session_prune::plan_all(&targets, &policy);
    let threshold = cfg.prune_confirm_threshold as usize;
    let report = if plan.len() >= threshold {
        tracing::info!(
            "housekeeping: {} entries prunable (>= threshold {}); \
             run `houyi cleanup` to review",
            plan.len(),
            threshold
        );
        PruneReport::default()
    } else {
        let report = session_prune::apply_prune(&plan);
        tracing::info!(
            "housekeeping: pruned {} entries, {} errors",
            report.removed,
            report.errors
        );
        report
    };

    write_marker(&marker_path, &report, plan.len());
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
/// A non-blocking try-flock on each lock file: success means no holder (the
/// probe releases immediately), failure (EAGAIN) means a live process holds
/// it, so the session is protected from prune. mtime recency does not cover
/// a held-but-idle session, so the probe is the real guard, not a
/// fallback. Unix-only (flock); non-unix returns empty (no cross-process
/// lock, single-process only).
#[cfg(unix)]
fn scan_lock_held_sessions(sessions_root: &Path) -> Vec<SessionId> {
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
        let Ok(file) = OpenOptions::new().write(true).open(&lock_path) else {
            continue;
        };
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(lock) => drop(lock),
            Err(_) => held.push(sid),
        }
    }
    held
}

#[cfg(not(unix))]
fn scan_lock_held_sessions(_sessions_root: &Path) -> Vec<SessionId> {
    Vec::new()
}
