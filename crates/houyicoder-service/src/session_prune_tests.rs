//! Peer tests for the session retention plan + apply: the TTL rule for
//! logged sessions, the sooner empty-session net, the count cap dropping
//! the oldest survivors, the protected-set sparing, snapshot + debug-log
//! rotation, and the stat-first recency listing the picker and --continue
//! rely on. Plan tests assert on the plan's content (plan_prune is
//! read-only); apply tests assert on the filesystem + the report.

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

/// An empty_ttl of 0 opts out of the empty-session net: a no-log session is
/// not pruned sooner than the logged TTL, mirroring the ttl=0 opt-out for
/// logged sessions. Guards against the inverted guard that checked the
/// logged ttl knob instead of the effective one.
#[test]
fn test_plan_empty_optout_zero() {
    let root = temp_root();
    let sid_empty = fresh_sid();
    let d_empty = session(&root, &sid_empty, false);
    age(&d_empty, 2 * 24 * 3600); // old, but empty_ttl=0 opts out
    let policy = PrunePolicy {
        empty_ttl_secs: 0,
        ..default_policy()
    };
    let plan = plan_prune(&root, &policy);
    assert!(
        entry_for(&plan, &sid_empty).is_none(),
        "empty_ttl=0 must opt out of the empty-session prune"
    );
    let _r = fs::remove_dir_all(&root);
}

/// A logged TTL of 0 opts out of the age-based prune for logged sessions:
/// only the count cap can drop them. Symmetry with empty_ttl=0.
#[test]
fn test_plan_ttl_optout_zero() {
    let root = temp_root();
    let sid = fresh_sid();
    let d = session(&root, &sid, true);
    age(&d.join("log.jsonl"), 31 * 24 * 3600); // past the default ttl, but ttl=0
    let policy = PrunePolicy {
        ttl_secs: 0,
        max_count: 0, // no cap either
        ..default_policy()
    };
    let plan = plan_prune(&root, &policy);
    assert!(
        entry_for(&plan, &sid).is_none(),
        "ttl=0 must opt out of the age-based prune"
    );
    let _r = fs::remove_dir_all(&root);
}

/// A logged TTL of 0 opts out of the logged prune ONLY -- the empty-session
/// net keeps its own ttl and still fires. The two knobs are independent, so
/// switching one off must not switch the other off with it.
///
/// This is the direction the previous test cannot reach: it asserts an
/// absence, which the guard that read the logged knob also produced, so it
/// passed before the fix as well. Only asserting that the empty net still
/// fires under ttl=0 discriminates -- the old guard evaluated the logged
/// knob, found 0, and skipped every session including the empty ones.
#[test]
fn test_ttl_zero_empty_net() {
    let root = temp_root();
    let sid_empty = fresh_sid();
    let d_empty = session(&root, &sid_empty, false);
    age(&d_empty, 2 * 24 * 3600); // past the 24h empty ttl
    let policy = PrunePolicy {
        ttl_secs: 0,        // logged prune off
        max_count: 0,       // cap off, so the empty net is the only rule left
        ..default_policy()  // empty_ttl_secs stays 24h
    };
    let plan = plan_prune(&root, &policy);
    let e = entry_for(&plan, &sid_empty)
        .expect("ttl=0 must not disable the empty-session net; the knobs are independent");
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

/// truncate_tail returns false when it cannot write the sibling tmp
/// (here: the parent dir is read-only, so the create-on-open fails). The
/// original file is left intact -- the crash-safe rename never ran. Covers
/// the cleanup branch (a crash mid-write must not corrupt the original).
#[cfg(unix)]
#[test]
fn test_apply_truncate_fails_safely() {
    use std::os::unix::fs::PermissionsExt;
    let root = temp_root();
    let log = root.join("debug.log");
    fs::write(&log, b"head-line-1\nsecond-line-2\n").expect("write log");
    // Make the parent read-only so the sibling .debug.log.tmp cannot be
    // created: open(create=true) fails, truncate_tail cleans up + returns
    // false without touching the original.
    fs::set_permissions(&root, fs::Permissions::from_mode(0o500)).expect("chmod ro");
    let ok = apply_prune(&PrunePlan {
        entries: plan_debug_log(&log, 15),
        kept: 0,
    });
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).expect("chmod rw");
    assert_eq!(ok.truncated, 0, "no truncation when the tmp write fails");
    assert!(ok.errors >= 1, "the failed truncate counts as an error");
    // The original is untouched: no half-written hybrid.
    let after = fs::read_to_string(&log).expect("read");
    assert_eq!(
        after, "head-line-1\nsecond-line-2\n",
        "original intact on failure"
    );
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
    assert_eq!(
        report.truncated, 1,
        "a truncation counts as truncated, not removed"
    );
    assert_eq!(report.removed, 0, "a truncation is not a removal");
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

/// A session without a log is skipped: /resume hard-errors on a missing
/// log, so a no-log row is one the user cannot act on -- it must not take
/// a visible slot.
#[test]
fn test_list_recent_skips_logless() {
    let root = temp_root();
    let with_log = fresh_sid();
    let no_log = fresh_sid();
    let d1 = session(&root, &with_log, true);
    age(&d1.join("log.jsonl"), 3600);
    let d2 = session(&root, &no_log, false);
    age(&d2, 1800);
    let got = list_recent_sessions(&root, 100);
    assert_eq!(got.len(), 1, "no-log session skipped");
    assert_eq!(got[0].0.to_string(), with_log);
    let _r = fs::remove_dir_all(&root);
}

/// Sorted newest-first and truncated to the limit: the caller relies on
/// the order (newest at [0]) and the limit (only the visible N pay the
/// sidecar parse).
#[test]
fn test_list_recent_newest_first() {
    let root = temp_root();
    let sids: Vec<String> = (0..3).map(|_| fresh_sid()).collect();
    for (i, sid) in sids.iter().enumerate() {
        let d = session(&root, sid, true);
        // i=0 newest (1h ago), i=2 oldest (3h ago).
        age(&d.join("log.jsonl"), (i as u64 + 1) * 3600);
    }
    let got = list_recent_sessions(&root, 2);
    assert_eq!(got.len(), 2, "limit applied");
    assert_eq!(got[0].0.to_string(), sids[0], "newest first");
    assert_eq!(got[1].0.to_string(), sids[1], "second-newest second");
    let _r = fs::remove_dir_all(&root);
}

#[test]
fn test_backlog_notice_over_cap() {
    let root = temp_root();
    for i in 0..3 {
        session(
            &root,
            &format!("00000000-0000-0000-0000-00000000000{i}"),
            true,
        );
    }
    let notice = store_backlog_notice(&root, 2).expect("3 dirs over cap 2");
    assert!(
        notice.contains("3 sessions") && notice.contains("over the retention count"),
        "notice states the size and the rule: {notice}"
    );
    assert!(
        notice.contains("houyi cleanup"),
        "notice points at the review path: {notice}"
    );
    let _r = fs::remove_dir_all(&root);
}

#[test]
fn test_backlog_notice_under_cap() {
    let root = temp_root();
    session(&root, &fresh_sid(), true);
    assert!(
        store_backlog_notice(&root, 2).is_none(),
        "1 dir under cap 2 is no backlog"
    );
    let _r = fs::remove_dir_all(&root);
}

#[test]
fn test_backlog_cap_zero() {
    let root = temp_root();
    for i in 0..3 {
        session(
            &root,
            &format!("00000000-0000-0000-0000-00000000000{i}"),
            true,
        );
    }
    assert!(
        store_backlog_notice(&root, 0).is_none(),
        "cap 0 opts out of the count rule, so out of the notice"
    );
    let _r = fs::remove_dir_all(&root);
}
