//! Session retention: plan + apply, two phases over the sid-keyed sessions
//! root. The shape mirrors SnapshotStore::prune (ttl, cap, protected) -
//! one prune vocabulary across the store - adapted for sessions: the cap
//! counts the user's own sessions (not bytes) and last-active is the log's
//! mtime, else the directory's. A shell - a directory a process created and
//! appended nothing to - is idle since that same mtime and pruned sooner via
//! empty_ttl_secs; a log whose sidecar never landed is not a shell, so it is
//! bounded by the full ttl.
//!
//! Two phases, not prune(dry_run): plan_prune is read-only (it decides what
//! to remove without touching anything), so its tests assert on the plan's
//! content, not on deletion. The plan is a value - the routing threshold, the
//! houyi-cleanup stdout, and a future /status all render the same PrunePlan.
//! apply_prune takes the plan and deletes; a separate step so a dry run is
//! free (plan + print, no apply) and the routing rule fires on plan size.
//!
//! Pure over the filesystem: the caller passes the root + the policy (ttl,
//! empty ttl, count cap, protected sessions). Error isolation is per-entry:
//! one bad dir does not stop the rest; failures count into the report and
//! surface via tracing, not stderr.

use std::path::{Path, PathBuf};

use houyicoder_context::SessionId;
use houyicoder_context::session_class::{SessionClass, scan_sessions};

/// The retention policy: how old before a session is prunable, how soon an
/// empty one is, the count cap, and the sessions a caller is live in (never
/// pruned, never counted toward the cap). Collected into a struct so a
/// caller does not pass five positional args that drift.
#[derive(Debug, Clone)]
pub struct PrunePolicy {
    /// A session older than this by last-active is prunable. 0 = no TTL
    /// prune (the count cap alone bounds the store).
    pub ttl_secs: u64,
    /// A session with no log older than this is prunable sooner. 0 = no
    /// empty-session net.
    pub empty_ttl_secs: u64,
    /// Remove the oldest survivors past this count. 0 = no count cap.
    pub max_count: usize,
    /// Sessions the caller is live in - never pruned, never counted.
    pub protected: Vec<SessionId>,
    /// Shell-snapshot files older than this are prunable (the crash-orphan
    /// net; the Drop path handles clean exits). 0 = no snapshot prune.
    pub snapshot_ttl_secs: u64,
    /// Truncate a debug log past this many bytes to its tail. 0 = no
    /// rotation. Debug logs are diagnostic; they rotate by size (keep the
    /// recent tail), not by age (age would lose the live scene).
    pub debug_max_bytes: u64,
}

/// What subsystem a prunable entry belongs to. Step one only produces
/// Session entries; snapshot and debug-log rotation land in the same plan
/// later, so the kind is here from the start rather than retrofitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PruneKind {
    Session,
    Snapshot,
    DebugLog,
}

/// Why an entry is slated for removal. Distinguishes the TTL rule, the
/// empty-session net, and the count-cap overflow so a rendering can tell
/// the user which rule caught each entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PruneReason {
    Ttl,
    EmptyTtl,
    CapOverflow,
}

/// What apply_prune does to an entry. Sessions and snapshots are removed
/// (dir vs file); a debug log is truncated to its tail (rotation, not
/// deletion - the live scene stays, the bulk goes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PruneAction {
    RemoveDir,
    RemoveFile,
    TruncateFile { keep_bytes: u64 },
}

/// One prunable target. The path is what apply_prune removes; the kind +
/// reason + last-active let a caller render the plan before deleting.
#[derive(Debug, Clone)]
pub struct PruneEntry {
    pub path: PathBuf,
    pub kind: PruneKind,
    pub reason: PruneReason,
    pub last_active: u64,
    pub action: PruneAction,
}

/// The decision: what to remove, and how many of the user's own sessions
/// survive. Only those are counted, so the number is smaller than the
/// directories left on disk whenever a child, a crash orphan, or a shell
/// survives outside the cap. A value - routing, dry-run, and /status render
/// the same plan.
#[derive(Debug, Clone, Default)]
pub struct PrunePlan {
    pub entries: Vec<PruneEntry>,
    pub kept: usize,
}

impl PrunePlan {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// How many entries were removed, how many debug logs were truncated to
/// their tail, and how many entries failed. A truncation is not a
/// removal -- the file stays (rotation, not deletion) -- so it counts
/// separately, else "removed N" overstates what vanished. A failure
/// counts once whatever the action was: one bad entry does not stop the
/// rest, and the count surfaces so a silent half-run is not hidden.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PruneReport {
    pub removed: usize,
    pub truncated: usize,
    pub errors: usize,
}

/// Read the sessions root and decide what to remove: a session older than
/// ttl by last-active (Ttl), a shell older than empty_ttl (EmptyTtl), then
/// the oldest of the user's own sessions past the count cap (CapOverflow).
/// The protected set is never pruned or counted. Read-only - nothing is
/// removed; pass the plan to apply_prune to execute.
pub fn plan_prune(root: &Path, policy: &PrunePolicy) -> PrunePlan {
    let now = now_secs();
    let mut entries: Vec<PruneEntry> = Vec::new();
    let mut kept: Vec<(PathBuf, u64)> = Vec::new();

    for session in scan_sessions(root) {
        if policy.protected.contains(&session.sid) {
            continue;
        }
        let last_active = session.last_active;
        let user = session.is_user_session();
        let shell = session.class == SessionClass::Shell;
        let ttl = if shell {
            policy.empty_ttl_secs
        } else {
            policy.ttl_secs
        };
        if ttl > 0 && now.saturating_sub(last_active) > ttl {
            entries.push(PruneEntry {
                path: session.path,
                kind: PruneKind::Session,
                reason: if shell {
                    PruneReason::EmptyTtl
                } else {
                    PruneReason::Ttl
                },
                last_active,
                action: PruneAction::RemoveDir,
            });
        } else if user {
            // The cap bounds the user's own sessions - the rows the picker
            // offers. A child, a crash orphan, and a shell are bounded by
            // their TTL window alone.
            kept.push((session.path, last_active));
        }
    }

    // Count cap: oldest survivors by last-active first, until under the cap.
    // Zero means no cap - the TTL rules alone bound the store.
    if policy.max_count > 0 && kept.len() > policy.max_count {
        kept.sort_by_key(|(_, la)| *la);
        let excess = kept.len() - policy.max_count;
        for (path, last_active) in kept.into_iter().take(excess) {
            entries.push(PruneEntry {
                path,
                kind: PruneKind::Session,
                reason: PruneReason::CapOverflow,
                last_active,
                action: PruneAction::RemoveDir,
            });
        }
        return PrunePlan {
            entries,
            kept: policy.max_count,
        };
    }
    PrunePlan {
        entries,
        kept: kept.len(),
    }
}

/// Execute a plan: remove every entry's path. Each removal is independent -
/// a Ctrl-C mid-run leaves a partial deletion that a re-run completes (the
/// plan is recomputed from the filesystem state, so already-removed entries
/// just are not there next time). No transaction: the independence is the
/// safety, written here so it is not an unrecorded assumption.
pub fn apply_prune(plan: &PrunePlan) -> PruneReport {
    let mut report = PruneReport::default();
    for entry in &plan.entries {
        let ok = match entry.action {
            PruneAction::RemoveDir => std::fs::remove_dir_all(&entry.path).is_ok(),
            PruneAction::RemoveFile => std::fs::remove_file(&entry.path).is_ok(),
            PruneAction::TruncateFile { keep_bytes } => truncate_tail(&entry.path, keep_bytes),
        };
        if ok {
            match entry.action {
                PruneAction::TruncateFile { .. } => report.truncated += 1,
                _ => report.removed += 1,
            }
        } else {
            report.errors += 1;
        }
    }
    report
}

/// Keep the last keep_bytes of a file, dropping the head. A debug log grows
/// at the tail (recent events), so the head is the stale half to drop.
/// Crash-safe: the tail is written to a sibling tmp file, fsynced, then
/// renamed over the original. An in-place rewrite (seek to 0, truncate,
/// write) leaves a half-written hybrid on a crash mid-write; the rename is
/// atomic, so a crash leaves either the old full file or the new tail,
/// never a corrupt mix.
fn truncate_tail(path: &Path, keep_bytes: u64) -> bool {
    use std::io::{Read, Seek, SeekFrom, Write};
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if metadata.len() <= keep_bytes {
        return true; // Already under the cap; nothing to do.
    }
    let start = metadata.len() - keep_bytes;
    let Ok(mut src) = std::fs::File::open(path) else {
        return false;
    };
    if src.seek(SeekFrom::Start(start)).is_err() {
        return false;
    }
    let mut tail = Vec::new();
    if src.read_to_end(&mut tail).is_err() {
        return false;
    }
    // Drop the leading partial line so the log stays line-aligned (a half-line
    // at the head reads as corrupt otherwise). The tail begins at a byte
    // boundary; walk forward to the next newline.
    if let Some(nl) = tail.iter().position(|&b| b == b'\n') {
        tail.drain(..=nl);
    }
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("log");
    // Per-writer tmp name (pid + process-local counter): two processes
    // rotating the same debug log must not share a tmp, or one rename
    // ships the other's half-written bytes. Nothing in this function's
    // signature stops two threads entering it for the same path, so the
    // counter keeps the name unique without the caller having to prove
    // they cannot.
    // A crash between create and rename leaves the tmp behind; nothing
    // reaps it today (the debug-log plan looks at the log itself, not its
    // siblings). One orphan per crashed rotation, so it is bounded by
    // crashes, not by time.
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_file_name(format!(".{file_name}.{}.{}.tmp", std::process::id(), seq));
    let cleanup = |tmp: &Path| {
        let _r = std::fs::remove_file(tmp);
        false
    };
    let Ok(mut out) = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&tmp)
    else {
        return cleanup(&tmp);
    };
    if out.write_all(&tail).is_err() {
        return cleanup(&tmp);
    }
    // fsync the tmp before rename so the renamed file's bytes are durable;
    // without it a crash after rename could expose unwritten pages.
    if out.sync_all().is_err() {
        return cleanup(&tmp);
    }
    drop(out);
    if std::fs::rename(&tmp, path).is_err() {
        return cleanup(&tmp);
    }
    true
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The roots the unified plan scans. Each is optional so a caller that
/// owns only some of them (a test, a headless run with no debug log) skips
/// the rest rather than pointing at a path that does not exist.
#[derive(Debug, Clone, Default)]
pub struct PruneTargets {
    pub sessions_root: Option<PathBuf>,
    pub shell_snapshots_root: Option<PathBuf>,
    pub debug_log: Option<PathBuf>,
}

/// Plan shell-snapshot files for removal by age. Each is a per-PID env
/// dump; a crash leaves orphans the Drop path never reaped. Files older
/// than the TTL are slated for RemoveFile.
pub fn plan_shell_snapshots(root: &Path, ttl_secs: u64) -> Vec<PruneEntry> {
    if ttl_secs == 0 {
        return Vec::new();
    }
    let now = now_secs();
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let last_active = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(now);
        if now.saturating_sub(last_active) > ttl_secs {
            out.push(PruneEntry {
                path,
                kind: PruneKind::Snapshot,
                reason: PruneReason::Ttl,
                last_active,
                action: PruneAction::RemoveFile,
            });
        }
    }
    out
}

/// Plan a debug log for tail-truncation if it exceeds the size cap. One
/// entry max (a single file); rotation keeps the recent tail, drops the
/// stale head. Returns empty when the file is under the cap or absent.
pub fn plan_debug_log(path: &Path, max_bytes: u64) -> Vec<PruneEntry> {
    if max_bytes == 0 {
        return Vec::new();
    }
    let Ok(metadata) = std::fs::metadata(path) else {
        return Vec::new();
    };
    if metadata.len() <= max_bytes {
        return Vec::new();
    }
    let last_active = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    vec![PruneEntry {
        path: path.to_path_buf(),
        kind: PruneKind::DebugLog,
        reason: PruneReason::Ttl,
        last_active,
        action: PruneAction::TruncateFile {
            keep_bytes: max_bytes,
        },
    }]
}

/// The unified plan: sessions + shell-snapshots + debug-log rotation, one
/// PrunePlan. The routing threshold, the cleanup subcommand stdout, and a
/// future /status all read this one value. A None target is skipped, so a
/// caller owns only the stores it has.
pub fn plan_all(targets: &PruneTargets, policy: &PrunePolicy) -> PrunePlan {
    let mut plan = if let Some(root) = &targets.sessions_root {
        plan_prune(root, policy)
    } else {
        PrunePlan::default()
    };
    if let Some(snap) = &targets.shell_snapshots_root {
        plan.entries
            .extend(plan_shell_snapshots(snap, policy.snapshot_ttl_secs));
    }
    if let Some(dbg) = &targets.debug_log {
        plan.entries
            .extend(plan_debug_log(dbg, policy.debug_max_bytes));
    }
    plan
}

/// Default empty-session TTL: a session whose durable log never landed (the
/// lazy-materialize crash window) is pruned sooner than a logged one. Shared
/// by the prune engine + the startup backlog notice so the two cannot drift
/// onto different defaults.
pub const EMPTY_TTL_SECS: u64 = 24 * 60 * 60;

/// Ceiling on the gap-range precise plan at startup, in scanned directories:
/// a store this size or smaller gets a real plan to catch a TTL backlog under
/// the count cap, larger ones are judged by the count route alone. A constant,
/// not the user-configurable cap: a user who raises the cap to keep more
/// sessions must not thereby pay a full stat on every launch.
pub const GAP_PRECISE_MAX_DIRS: usize = 2000;

/// Whether a store of this many directories gets the gap route's precise plan.
/// At or under the ceiling a plan catches a TTL backlog the count route cannot
/// see; above it the count route judges alone, so a launch never pays a second
/// pass over a store that large.
fn gap_route_taken(dir_count: usize) -> bool {
    dir_count <= GAP_PRECISE_MAX_DIRS
}

/// The startup backlog notice. Two routes, no drift between them:
/// - over the count cap: a count notice naming the store size (the count of
///   the user's own sessions, the class the picker lists from, so neither can
///   name a session the other does not; the picker shows a bounded prefix of
///   them, not a different set).
/// - in the gap range (above the routing threshold, under the ceiling): a
///   precise plan decides whether a TTL backlog exists. The notice then
///   carries no number: the gap policy is approximate (no lock-held scan), so
///   a prunable count here could disagree with cleanup's authoritative plan.
///   The notice routes; cleanup counts.
///
/// The routing input is the scanned directory total, not the user count: a
/// store whose backlog is sub-agent children or crash orphans holds thousands
/// of prunable directories and almost no user sessions, and that store is the
/// one the cleanup pointer is for. The number above stays on user sessions,
/// so the routing change does not restate it.
///
/// Above the ceiling the gap route is not taken and the count route is the
/// only judge, so a store that large whose backlog is children or orphans and
/// whose user count is under the cap is reported by neither route. The ceiling
/// is a cost guard -- the precise plan is a second pass over the store -- and
/// that hole is accepted for now rather than paid for on every launch.
///
/// cap 0 (count rule opted out) yields None, and an unreadable root counts
/// zero.
pub fn store_backlog_notice(
    sessions_root: &Path,
    cap: usize,
    threshold: usize,
    policy: &PrunePolicy,
) -> Option<String> {
    if cap == 0 {
        return None;
    }
    let sessions = scan_sessions(sessions_root);
    let count = sessions
        .iter()
        .filter(|session| session.is_user_session())
        .count();
    if count > cap {
        return Some(format!(
            "session store holds {count} sessions, over the retention count \
             of {cap} — run houyi cleanup to review"
        ));
    }
    if sessions.len() > threshold && gap_route_taken(sessions.len()) {
        let plan = plan_prune(sessions_root, policy);
        if plan.len() >= threshold {
            return Some(
                "sessions are past their retention window — run houyi cleanup to review".into(),
            );
        }
    }
    None
}

#[cfg(test)]
#[path = "session_prune_tests.rs"]
mod tests;
