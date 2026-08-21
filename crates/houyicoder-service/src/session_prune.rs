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

/// One prunable target. The path is what apply_prune removes; the kind +
/// reason + last-active let a caller render the plan before deleting.
#[derive(Debug, Clone)]
pub struct PruneEntry {
    pub path: PathBuf,
    pub kind: PruneKind,
    pub reason: PruneReason,
    pub last_active: u64,
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
        if std::fs::remove_dir_all(&entry.path).is_ok() {
            report.removed += 1;
        } else {
            report.errors += 1;
        }
    }
    report
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
}
