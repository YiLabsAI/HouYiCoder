//! Startup-notice tests for the session store: which route judges a store,
//! what the count names, and when the notice stays silent. Split from the
//! retention tests on size and subject grounds: the routes read the plan the
//! retention engine produces, so the two change for different reasons.

use super::*;

/// The count bounds the user's own sessions: a child has a log and a sidecar
/// like any other session, but reporting it would name a store size the user
/// cannot reconcile with the rows they see.
#[test]
fn test_count_excludes_child() {
    let root = temp_root();
    session(&root, &fresh_sid(), true);
    session(&root, &fresh_sid(), true);
    child_session(&root, &fresh_sid());
    assert!(
        store_backlog_notice(&root, 2, 100, &default_policy()).is_none(),
        "two user sessions at cap 2 are no backlog; the child must not count"
    );
    let _r = fs::remove_dir_all(&root);
}
/// The size the startup notice names and the rows the picker shows are two
/// presentations of one count, so they agree on a store that holds all four
/// directory shapes. This is the property the split classifiers broke: the
/// notice said 1000 while the picker listed fewer.
#[test]
fn test_notice_count_matches_rows() {
    let root = temp_root();
    for _ in 0..3 {
        session(&root, &fresh_sid(), true);
    }
    child_session(&root, &fresh_sid());
    write_session(&root, &fresh_sid(), false, SessionProvenance::Fresh);
    fs::create_dir_all(root.join("index")).expect("non-session dir");

    let notice = store_backlog_notice(&root, 2, 100, &default_policy()).expect("over cap");
    assert!(
        notice.contains("holds 3 sessions"),
        "the notice names the user's own sessions only: {notice}"
    );
    assert_eq!(
        recent_user_sessions(&root, 100).len(),
        3,
        "the picker shows exactly the sessions the notice counted"
    );
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
    // Over the count cap: the count route names the store size. threshold is
    // irrelevant on this route (count > cap wins first).
    let notice = store_backlog_notice(&root, 2, 100, &default_policy()).expect("3 dirs over cap 2");
    assert!(
        notice.contains("3 sessions") && notice.contains("over the retention count"),
        "count route states the size and the rule: {notice}"
    );
    assert!(
        notice.contains("houyi cleanup"),
        "notice points at the review path: {notice}"
    );
    let _r = fs::remove_dir_all(&root);
}
/// The gap route fires on the store's directories, not on the user's sessions:
/// a backlog of expired children is prunable work the user should hear about,
/// and counting only their own sessions would leave it silent.
#[test]
fn test_backlog_routes_on_dirs() {
    let root = temp_root();
    session(&root, &fresh_sid(), true);
    for _ in 0..3 {
        let child = child_session(&root, &fresh_sid());
        age(&child.join("log.jsonl"), 31 * 24 * 3600);
    }
    let notice = store_backlog_notice(&root, 2, 3, &default_policy())
        .expect("4 directories over threshold 3 with 3 past their window");
    assert!(
        notice.contains("retention window"),
        "the gap route carries no number, it routes: {notice}"
    );
    // The same shape with nothing expired is silent: the route fires on size,
    // but the notice still needs prunable work behind it.
    let fresh = temp_root();
    session(&fresh, &fresh_sid(), true);
    for _ in 0..3 {
        child_session(&fresh, &fresh_sid());
    }
    assert!(
        store_backlog_notice(&fresh, 2, 3, &default_policy()).is_none(),
        "size alone is not a backlog"
    );
    let _r = fs::remove_dir_all(&root);
    let _r = fs::remove_dir_all(&fresh);
}
#[test]
fn test_backlog_notice_under_cap() {
    let root = temp_root();
    session(&root, &fresh_sid(), true);
    assert!(
        store_backlog_notice(&root, 2, 100, &default_policy()).is_none(),
        "1 dir under cap 2 and under threshold 100 is no backlog"
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
        store_backlog_notice(&root, 0, 100, &default_policy()).is_none(),
        "cap 0 opts out of the count rule, so out of the notice"
    );
    let _r = fs::remove_dir_all(&root);
}
/// The gap range (above threshold, at or under cap): a TTL-expired backlog
/// the count route misses (under cap) is caught by a precise plan. The notice
/// carries no number - the gap policy is approximate (no lock-held scan), so
/// a prunable count here could disagree with cleanup's authoritative plan.
#[test]
fn test_backlog_gap_ttl_backlog() {
    let root = temp_root();
    // 150 sessions, all past the 30d TTL, store under the 1000 cap.
    for i in 0..150 {
        let d = session(&root, &format!("00000000-0000-0000-0000-{i:012x}"), true);
        age(&d.join("log.jsonl"), 31 * 24 * 3600);
    }
    let notice = store_backlog_notice(&root, 1000, 100, &default_policy())
        .expect("150 TTL-expired sessions in the gap range fire the notice");
    assert!(
        notice.contains("retention window") && notice.contains("houyi cleanup"),
        "gap notice routes without a number: {notice}"
    );
    assert!(
        !notice.contains("150"),
        "no prunable count in the gap notice (would drift with cleanup): {notice}"
    );
    let _r = fs::remove_dir_all(&root);
}
/// The ceiling is a bound on the directory total, inclusive at the ceiling.
#[test]
fn test_gap_route_ceiling_bounds() {
    let ceiling = crate::session_prune::GAP_PRECISE_MAX_DIRS;
    assert!(
        gap_route_taken(ceiling),
        "a store at the ceiling still gets the precise plan"
    );
    assert!(
        !gap_route_taken(ceiling + 1),
        "one directory past it, the count route judges alone"
    );
}

/// A store of a given size whose backlog is the first directories: logs past
/// the TTL, then fresh shells filling it out. No route can name a user
/// session, so only the gap route can speak.
fn store_with_backlog(root: &Path, total: usize, expired: usize) {
    for i in 0..expired {
        let dir = log_only_session(root, &format!("00000000-0000-0000-0000-{i:012x}"));
        age(&dir.join("log.jsonl"), 31 * 24 * 3600);
    }
    for i in expired..total {
        fs::create_dir_all(root.join(format!("00000000-0000-0000-0000-{i:012x}")))
            .expect("mkdir shell");
    }
}

/// The notice is what consults the ceiling, on the directory total: past the
/// ceiling the same backlog that fires the notice on a small store is judged
/// by the count route alone. A store whose precise plan would have found
/// nothing could not tell a live ceiling from a deleted one, so the backlog is
/// real and only the size differs.
#[test]
fn test_gap_ceiling_bounds_notice() {
    let root = temp_root();
    store_with_backlog(&root, crate::session_prune::GAP_PRECISE_MAX_DIRS + 1, 100);
    assert!(
        store_backlog_notice(&root, 1000, 100, &default_policy()).is_none(),
        "past the ceiling this backlog is judged by the count route alone"
    );
    let _r = fs::remove_dir_all(&root);
}

/// A non-session subdirectory (index/, a stray) is not counted: the size the
/// count route names must be honest, or the notice overstates the store.
#[test]
fn test_backlog_skips_non_session() {
    let root = temp_root();
    fs::create_dir_all(root.join("index")).unwrap(); // not a SessionId
    for i in 0..3 {
        session(
            &root,
            &format!("00000000-0000-0000-0000-00000000000{i}"),
            true,
        );
    }
    let notice = store_backlog_notice(&root, 2, 100, &default_policy()).unwrap();
    assert!(
        notice.contains("3 sessions"),
        "non-session dirs excluded from the count: {notice}"
    );
    let _r = fs::remove_dir_all(&root);
}
/// A no-log session is a crash orphan, not a resumable session. It must not
/// inflate the count the startup notice names — otherwise the notice fires
/// over the cap while the resume picker (which requires a log) shows far
/// fewer, and the two disagree.
#[test]
fn test_backlog_excludes_logless() {
    let root = temp_root();
    session(&root, &fresh_sid(), true);
    session(&root, &fresh_sid(), true);
    let d_empty = session(&root, &fresh_sid(), false); // no log
    age(&d_empty, 1800); // recent, within empty_ttl
    // 2 logged sessions, 1 logless; cap=2. The logless one does not count.
    assert!(
        store_backlog_notice(&root, 2, 100, &default_policy()).is_none(),
        "no-log session must not inflate the count past the cap"
    );
    let _r = fs::remove_dir_all(&root);
}
