//! Session retention: plan + apply, two phases over the sid-keyed sessions
//! root. The shape mirrors SnapshotStore::prune (ttl, cap, protected) -
//! one prune vocabulary across the store - adapted for sessions: the cap
//! counts sessions (not bytes), last-active is the log.jsonl mtime, and a
//! session with no log is idle since its dir mtime, pruned sooner via
//! empty_ttl_secs (the empty-session net for the lazy-materialize crash
//! window: a turn's durable log landed but the sidecar did not).
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

/// The decision: what to remove, and how many sessions survive the TTL pass
/// (for cap context + "N of M kept" rendering). A value - routing, dry-run,
/// and /status render the same plan.
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

/// How many entries were removed, and how many dir removes errored. Errors
/// are best-effort: a prune that fails on one entry still removes the rest,
/// and the count surfaces via tracing so a silent half-run is not hidden.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PruneReport {
    pub removed: usize,
    pub errors: usize,
}

/// Read the sessions root and decide what to remove: a session older than
/// ttl by last-active (Ttl), one with no log older than empty_ttl (EmptyTtl),
/// then the oldest survivors past the count cap (CapOverflow). The
/// protected set is never pruned or counted. Read-only - nothing is removed;
/// pass the plan to apply_prune to execute.
pub fn plan_prune(root: &Path, policy: &PrunePolicy) -> PrunePlan {
    let now = now_secs();
    let mut entries: Vec<PruneEntry> = Vec::new();
    let mut kept: Vec<(PathBuf, u64)> = Vec::new();

    let Ok(dir_entries) = std::fs::read_dir(root) else {
        return PrunePlan::default();
    };
    for entry in dir_entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(sid_str) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // A dir whose name is not a session id (an index/ sub-dir, a stray)
        // is not a session - skip it from both prune and cap.
        let Some(sid) = SessionId::from_display_string(sid_str) else {
            continue;
        };
        if policy.protected.contains(&sid) {
            continue;
        }
        let has_log = path.join("log.jsonl").is_file();
        let last_active = if has_log {
            log_mtime(&path).unwrap_or_else(|| dir_mtime(&path))
        } else {
            dir_mtime(&path)
        };
        let ttl = if has_log {
            policy.ttl_secs
        } else {
            policy.empty_ttl_secs
        };
        if policy.ttl_secs > 0 && now.saturating_sub(last_active) > ttl {
            entries.push(PruneEntry {
                path,
                kind: PruneKind::Session,
                reason: if has_log {
                    PruneReason::Ttl
                } else {
                    PruneReason::EmptyTtl
                },
                last_active,
                action: PruneAction::RemoveDir,
            });
        } else {
            kept.push((path, last_active));
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
            report.removed += 1;
        } else {
            report.errors += 1;
        }
    }
    report
}

/// Keep the last keep_bytes of a file, dropping the head. A debug log grows
/// at the tail (recent events), so the head is the stale half to drop.
fn truncate_tail(path: &Path, keep_bytes: u64) -> bool {
    use std::io::{Read, Seek, SeekFrom, Write};
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if metadata.len() <= keep_bytes {
        return true; // Already under the cap; nothing to do.
    }
    let Ok(mut f) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    else {
        return false;
    };
    let start = metadata.len() - keep_bytes;
    if f.seek(SeekFrom::Start(start)).is_err() {
        return false;
    }
    let mut tail = Vec::new();
    if f.read_to_end(&mut tail).is_err() {
        return false;
    }
    // Drop the leading partial line so the log stays line-aligned (a
    // half-line at the head reads as corrupt otherwise). The tail begins at
    // a byte boundary; walk forward to the next newline.
    if let Some(nl) = tail.iter().position(|&b| b == b'\n') {
        tail = tail[nl + 1..].to_vec();
    }
    if f.seek(SeekFrom::Start(0)).is_err() {
        return false;
    }
    if f.set_len(keep_bytes).is_err() || f.write_all(&tail).is_err() {
        return false;
    }
    let _r = f.set_len(tail.len() as u64);
    true
}

fn log_mtime(dir: &Path) -> Option<u64> {
    let m = std::fs::metadata(dir.join("log.jsonl")).ok()?;
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

fn dir_mtime(dir: &Path) -> u64 {
    std::fs::metadata(dir)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
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

/// List sessions by last-active, stat-only (no sidecar parse). Returns
/// (sid, last_active_secs) sorted newest-first, limited to the top N.
/// The caller parses only these N sidecars (read_meta), not all -- on a
/// 50k backlog the stat phase is readdir + one metadata() per dir (fast,
/// no JSON), and only the visible N pay the serde cost. last-active is
/// the log.jsonl mtime, falling back to the dir mtime when no log exists
/// (a session that never appended).
pub fn list_recent_sessions(root: &Path, limit: usize) -> Vec<(SessionId, u64)> {
    let now = now_secs();
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut sessions: Vec<(SessionId, u64)> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if !path.is_dir() {
                return None;
            }
            let sid_str = path.file_name()?.to_str()?;
            let sid = SessionId::from_display_string(sid_str)?;
            let last_active = log_mtime(&path).unwrap_or_else(|| dir_mtime(&path));
            Some((sid, last_active))
        })
        .collect();
    sessions.sort_by_key(|(_, la)| std::cmp::Reverse(*la));
    sessions.truncate(limit);
    let _ = now;
    sessions
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    use houyicoder_context::SessionId;

    fn temp_root() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("houyi-plan-{seq}-{}", std::process::id()));
        let _r = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).expect("mkdir root");
        d
    }

    /// Stamp a path's mtime to N seconds ago so the TTL rule sees it as old.
    fn age(path: &Path, secs_ago: u64) {
        let t = SystemTime::now() - Duration::from_secs(secs_ago);
        let f = fs::File::open(path).expect("open");
        f.set_times(fs::FileTimes::new().set_modified(t))
            .expect("set mtime");
    }

    fn session(root: &Path, sid: &str, with_log: bool) -> PathBuf {
        let d = root.join(sid);
        fs::create_dir_all(&d).expect("mkdir sid");
        fs::write(d.join("session.json"), "{}").expect("sidecar");
        if with_log {
            fs::write(d.join("log.jsonl"), "[]").expect("log");
        }
        d
    }

    fn fresh_sid() -> String {
        SessionId::new().to_string()
    }

    fn default_policy() -> PrunePolicy {
        PrunePolicy {
            ttl_secs: 30 * 24 * 3600,
            empty_ttl_secs: 24 * 3600,
            max_count: 1000,
            protected: Vec::new(),
            snapshot_ttl_secs: 7 * 24 * 3600,
            debug_max_bytes: 10 * 1024 * 1024,
        }
    }

    /// A plan entry for a given sid dir, if present.
    fn entry_for<'a>(plan: &'a PrunePlan, sid: &str) -> Option<&'a PruneEntry> {
        plan.entries
            .iter()
            .find(|e| e.path.file_name().and_then(|n| n.to_str()) == Some(sid))
    }

    #[test]
    fn test_plan_old_logged() {
        let root = temp_root();
        let sid = fresh_sid();
        let d = session(&root, &sid, true);
        age(&d.join("log.jsonl"), 31 * 24 * 3600); // past the 30d ttl
        let plan = plan_prune(&root, &default_policy());
        let e = entry_for(&plan, &sid).expect("old logged session is prunable");
        assert_eq!(e.reason, PruneReason::Ttl);
        let _r = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_plan_keeps_recent() {
        let root = temp_root();
        let sid = fresh_sid();
        let d = session(&root, &sid, true);
        age(&d.join("log.jsonl"), 3600); // within the 30d ttl
        let plan = plan_prune(&root, &default_policy());
        assert!(plan.entries.is_empty());
        assert_eq!(plan.kept, 1);
        let _r = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_plan_empty_shorter_ttl() {
        let root = temp_root();
        // Two sessions equally old (2 days): one with a log (within 30d),
        // one without (past the 24h empty ttl). Only the empty one is prunable.
        let sid_log = fresh_sid();
        let d_log = session(&root, &sid_log, true);
        age(&d_log.join("log.jsonl"), 2 * 24 * 3600);
        let sid_empty = fresh_sid();
        let d_empty = session(&root, &sid_empty, false);
        age(&d_empty, 2 * 24 * 3600);
        let plan = plan_prune(&root, &default_policy());
        assert!(entry_for(&plan, &sid_log).is_none());
        let e = entry_for(&plan, &sid_empty).expect("empty old session is prunable");
        assert_eq!(e.reason, PruneReason::EmptyTtl);
        let _r = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_plan_protected_spared() {
        let root = temp_root();
        let sid = fresh_sid();
        let d = session(&root, &sid, true);
        age(&d.join("log.jsonl"), 31 * 24 * 3600); // past ttl
        let protected = SessionId::from_display_string(&sid).expect("sid");
        let policy = PrunePolicy {
            protected: vec![protected],
            ..default_policy()
        };
        let plan = plan_prune(&root, &policy);
        assert!(entry_for(&plan, &sid).is_none());
        let _r = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_plan_cap_drops_oldest() {
        let root = temp_root();
        // Three recent sessions (within ttl), cap at 1: the oldest two drop.
        let sids: Vec<String> = (0..3).map(|_| fresh_sid()).collect();
        for (i, sid) in sids.iter().enumerate() {
            let d = session(&root, sid, true);
            // Older i = older mtime: 1h, 2h, 3h ago.
            age(&d.join("log.jsonl"), (i as u64 + 1) * 3600);
        }
        let policy = PrunePolicy {
            max_count: 1,
            ..default_policy()
        };
        let plan = plan_prune(&root, &policy);
        assert_eq!(plan.entries.len(), 2);
        // The newest (1h ago, i=0) survives; the other two are prunable.
        assert!(entry_for(&plan, &sids[0]).is_none());
        assert!(entry_for(&plan, &sids[1]).is_some());
        assert!(entry_for(&plan, &sids[2]).is_some());
        for e in &plan.entries {
            assert_eq!(e.reason, PruneReason::CapOverflow);
        }
        let _r = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_plan_skips_non_session() {
        let root = temp_root();
        // An index/ sub-dir is not a session id - never prunable, even if old.
        let idx = root.join("index");
        fs::create_dir_all(&idx).expect("mkdir index");
        age(&idx, 100 * 24 * 3600);
        let plan = plan_prune(&root, &default_policy());
        assert!(plan.entries.is_empty());
        let _r = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_apply_removes_entries() {
        let root = temp_root();
        let sid = fresh_sid();
        let d = session(&root, &sid, true);
        age(&d.join("log.jsonl"), 31 * 24 * 3600);
        let plan = plan_prune(&root, &default_policy());
        let report = apply_prune(&plan);
        assert_eq!(report.removed, 1);
        assert!(!d.exists());
        let _r = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_apply_empty_plan_noop() {
        let plan = PrunePlan::default();
        let report = apply_prune(&plan);
        assert_eq!(report, PruneReport::default());
    }

    #[test]
    fn test_plan_snapshots_old_file() {
        let root = temp_root();
        let snap = root.join("snapshot-sh-123-456.sh");
        fs::write(&snap, "env").expect("write snap");
        age(&snap, 8 * 24 * 3600); // past the 7d snapshot ttl
        let entries = plan_shell_snapshots(&root, 7 * 24 * 3600);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, PruneKind::Snapshot);
        assert_eq!(entries[0].action, PruneAction::RemoveFile);
        let _r = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_plan_snapshots_keeps_recent() {
        let root = temp_root();
        let snap = root.join("snapshot-sh-123-456.sh");
        fs::write(&snap, "env").expect("write snap");
        age(&snap, 3600); // 1h, within 7d
        let entries = plan_shell_snapshots(&root, 7 * 24 * 3600);
        assert!(entries.is_empty());
        let _r = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_plan_debug_under_cap() {
        let root = temp_root();
        let log = root.join("debug.log");
        fs::write(&log, b"small").expect("write log");
        let entries = plan_debug_log(&log, 10 * 1024 * 1024);
        assert!(entries.is_empty(), "under the cap = no rotation");
        let _r = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_plan_debug_over_cap() {
        let root = temp_root();
        let log = root.join("debug.log");
        // 20 bytes content, cap at 10: over the cap.
        fs::write(&log, b"0123456789\nfirst\nsecond\n").expect("write log");
        let entries = plan_debug_log(&log, 10);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, PruneKind::DebugLog);
        assert!(matches!(
            entries[0].action,
            PruneAction::TruncateFile { .. }
        ));
        let _r = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_apply_truncates_to_tail() {
        let root = temp_root();
        let log = root.join("debug.log");
        // Two lines; cap keeps the tail, line-aligned.
        fs::write(&log, b"head-line-1\nsecond-line-2\n").expect("write log");
        let entries = plan_debug_log(&log, 15);
        let plan = PrunePlan { entries, kept: 0 };
        let report = apply_prune(&plan);
        assert_eq!(report.removed, 1);
        let after = fs::read_to_string(&log).expect("read");
        assert!(
            after.starts_with("second-line"),
            "tail kept, head dropped: {after}"
        );
        let _r = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_plan_all_merges_three() {
        let root = temp_root();
        // One old session + one old snapshot + (debug under cap, skipped).
        let sid = fresh_sid();
        let d = session(&root, &sid, true);
        age(&d.join("log.jsonl"), 31 * 24 * 3600);
        let snap_dir = root.join("shell-snapshots");
        fs::create_dir_all(&snap_dir).expect("mkdir snaps");
        let snap = snap_dir.join("snapshot-sh-1-2.sh");
        fs::write(&snap, "env").expect("write snap");
        age(&snap, 8 * 24 * 3600);
        let targets = PruneTargets {
            sessions_root: Some(root.clone()),
            shell_snapshots_root: Some(snap_dir),
            debug_log: None,
        };
        let plan = plan_all(&targets, &default_policy());
        assert_eq!(plan.entries.len(), 2);
        assert_eq!(
            plan.entries
                .iter()
                .filter(|e| e.kind == PruneKind::Session)
                .count(),
            1
        );
        assert_eq!(
            plan.entries
                .iter()
                .filter(|e| e.kind == PruneKind::Snapshot)
                .count(),
            1
        );
        let _r = fs::remove_dir_all(&root);
    }
}
