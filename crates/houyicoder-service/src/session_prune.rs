//! Session retention: prune the sid-keyed sessions root under a TTL + a
//! count cap, protecting the live session. The shape mirrors
//! SnapshotStore::prune (ttl, cap, protected) - one prune vocabulary across
//! the store - adapted for sessions: the cap counts sessions (not bytes),
//! and last-active is the log.jsonl mtime. A session with no log is idle
//! since its dir mtime and is pruned sooner via empty_ttl_secs - the
//! empty-session safety net for the case where a turn's durable log lands
//! but the sidecar does not (a crash in the lazy-materialize window).
//!
//! Pure over the filesystem: the caller passes the root + the current
//! session id to protect. Error isolation is per-session: a bad dir does not
//! stop the sweep; failures count into the result and surface via tracing,
//! not stderr.

use std::path::{Path, PathBuf};

use houyicoder_context::SessionId;

/// How many sessions were pruned, and how many dir reads/removes errored.
/// Errors are best-effort: a prune that fails on one session still prunes
/// the others, and the count surfaces via tracing so a silent half-run is
/// not hidden.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PruneResult {
    pub removed: usize,
    pub errors: usize,
}

/// Prune the sessions root: a session older than ttl_secs by last-active is
/// removed; a session with no log older than empty_ttl_secs is removed
/// sooner (the empty-session net); then the oldest survivors are removed
/// until the count is at or under max_count. The protected set (the
/// current/live session) is never pruned or counted. Returns removed +
/// error counts. Idempotent: a second run removes nothing the first did not.
pub fn prune_sessions(
    root: &Path,
    ttl_secs: u64,
    empty_ttl_secs: u64,
    max_count: usize,
    protected: &[SessionId],
) -> PruneResult {
    let now = now_secs();
    let mut result = PruneResult::default();
    let mut kept: Vec<(PathBuf, u64)> = Vec::new();

    let Ok(entries) = std::fs::read_dir(root) else {
        return result;
    };
    for entry in entries.flatten() {
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
        if protected.contains(&sid) {
            continue;
        }
        let has_log = path.join("log.jsonl").is_file();
        let last_active = if has_log {
            log_mtime(&path).unwrap_or_else(|| dir_mtime(&path))
        } else {
            dir_mtime(&path)
        };
        let ttl = if has_log { ttl_secs } else { empty_ttl_secs };
        if now.saturating_sub(last_active) > ttl {
            if std::fs::remove_dir_all(&path).is_ok() {
                result.removed += 1;
            } else {
                result.errors += 1;
            }
        } else {
            kept.push((path, last_active));
        }
    }

    // Count cap: oldest by last-active first, until under the cap. Zero
    // means no cap - the TTL rules alone bound the store.
    if max_count > 0 && kept.len() > max_count {
        kept.sort_by_key(|(_, la)| *la);
        let excess = kept.len() - max_count;
        for (path, _) in kept.into_iter().take(excess) {
            if std::fs::remove_dir_all(&path).is_ok() {
                result.removed += 1;
            } else {
                result.errors += 1;
            }
        }
    }
    result
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
        let d = std::env::temp_dir().join(format!("houyi-prune-{seq}-{}", std::process::id()));
        let _r = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).expect("mkdir root");
        d
    }

    /// Stamp a path's mtime to N seconds ago so the TTL rule sees it as old.
    /// Opens read-only so it works on directories too (futimens needs write
    /// permission, which the test has on paths it created, not a writable fd).
    fn age(path: &Path, secs_ago: u64) {
        let t = SystemTime::now() - Duration::from_secs(secs_ago);
        let f = fs::File::open(path).expect("open");
        f.set_times(fs::FileTimes::new().set_modified(t))
            .expect("set mtime");
    }

    /// A session dir under root with optional log.jsonl + session.json.
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

    #[test]
    fn test_prune_old_logged_removed() {
        let root = temp_root();
        let sid = fresh_sid();
        let d = session(&root, &sid, true);
        age(&d.join("log.jsonl"), 31 * 24 * 3600); // 31 days, past the 30d ttl
        let r = prune_sessions(&root, 30 * 24 * 3600, 24 * 3600, 1000, &[]);
        assert_eq!(r.removed, 1);
        assert!(!d.exists());
        let _r = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_prune_keeps_recent() {
        let root = temp_root();
        let sid = fresh_sid();
        let d = session(&root, &sid, true);
        age(&d.join("log.jsonl"), 3600); // 1 hour, within the 30d ttl
        let r = prune_sessions(&root, 30 * 24 * 3600, 24 * 3600, 1000, &[]);
        assert_eq!(r.removed, 0);
        assert!(d.exists());
        let _r = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_prune_empty_shorter_ttl() {
        let root = temp_root();
        // Two sessions equally old (2 days): one with a log (within 30d),
        // one without (past the 24h empty ttl). Only the empty one prunes.
        let sid_log = fresh_sid();
        let d_log = session(&root, &sid_log, true);
        age(&d_log.join("log.jsonl"), 2 * 24 * 3600);
        let sid_empty = fresh_sid();
        let d_empty = session(&root, &sid_empty, false);
        age(&d_empty, 2 * 24 * 3600);
        let r = prune_sessions(&root, 30 * 24 * 3600, 24 * 3600, 1000, &[]);
        assert_eq!(r.removed, 1);
        assert!(d_log.exists());
        assert!(!d_empty.exists());
        let _r = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_prune_protected_spared() {
        let root = temp_root();
        let sid = fresh_sid();
        let d = session(&root, &sid, true);
        age(&d.join("log.jsonl"), 31 * 24 * 3600); // past ttl
        let protected = SessionId::from_display_string(&sid).expect("sid");
        let r = prune_sessions(&root, 30 * 24 * 3600, 24 * 3600, 1000, &[protected]);
        assert_eq!(r.removed, 0);
        assert!(d.exists());
        let _r = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_prune_cap_drops_oldest() {
        let root = temp_root();
        // Three recent sessions (within ttl), cap at 1: the oldest two drop.
        let sids: Vec<String> = (0..3).map(|_| fresh_sid()).collect();
        for (i, sid) in sids.iter().enumerate() {
            let d = session(&root, sid, true);
            // Older i = older mtime: 1h, 2h, 3h ago.
            age(&d.join("log.jsonl"), (i as u64 + 1) * 3600);
        }
        let r = prune_sessions(&root, 30 * 24 * 3600, 24 * 3600, 1, &[]);
        assert_eq!(r.removed, 2);
        // The newest (1h ago, i=0) survives.
        assert!(root.join(&sids[0]).exists());
        assert!(!root.join(&sids[1]).exists());
        assert!(!root.join(&sids[2]).exists());
        let _r = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_prune_skips_non_session() {
        let root = temp_root();
        // An index/ sub-dir is not a session id - never pruned, even if old.
        let idx = root.join("index");
        fs::create_dir_all(&idx).expect("mkdir index");
        age(&idx, 100 * 24 * 3600);
        let r = prune_sessions(&root, 30 * 24 * 3600, 24 * 3600, 1000, &[]);
        assert_eq!(r.removed, 0);
        assert!(idx.exists());
        let _r = fs::remove_dir_all(&root);
    }
}
