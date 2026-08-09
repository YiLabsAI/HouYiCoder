//! Progressive resume-resolution tests for poll_agent: the per-frame
//! resolver fills in picker row titles + last-active lazily, a few rows per
//! frame, so /resume opens instantly even with hundreds of sessions. Split
//! out of run_control_tests.rs on size grounds.
use crate::composition;
use crate::resume_picker::{SessionLister, SessionRow};
use crate::state::Pane;
use std::sync::Arc;

/// A lister whose resolve_detail prepends "resolved-" to the title so the
/// test can count touched rows. list_sessions returns N unresolved rows.
struct ResolvingLister(usize);
impl SessionLister for ResolvingLister {
    fn list_sessions(&self, _current: &str) -> Vec<SessionRow> {
        (0..self.0)
            .map(|i| SessionRow {
                sid_str: format!("sid{i}"),
                title: format!("row{i}"),
                cwd_basename: "repo".into(),
                last_active: i as u64,
                ..Default::default()
            })
            .collect()
    }
    fn resolve_detail(&self, row: &mut SessionRow) {
        row.title = format!("resolved-{}", row.title);
    }
}

/// Progressive resume resolution: poll_agent resolves at most 3 unresolved
/// picker rows per frame (so the picker opens instantly and fills in real
/// titles + last-active top-to-bottom over a few frames). The picker must be
/// open for the branch to fire; a closed picker resolves nothing.
#[test]
fn test_poll_resolves_rows_progressively() {
    let mut app = composition::app();
    app.session_lister = Some(Arc::new(ResolvingLister(5)));
    // Open the picker + load rows (same path as /resume with no arg).
    app.resume_picker.rows = app.session_lister.clone().unwrap().list_sessions("");
    app.resume_picker.open();
    app.pane = Pane::Resume;
    assert!(app.resume_picker.resolved.is_empty());
    // One poll resolves at most 3 rows (the per-frame cap).
    app.poll_agent();
    let touched = app
        .resume_picker
        .rows
        .iter()
        .filter(|r| r.title.starts_with("resolved-"))
        .count();
    assert_eq!(touched, 3, "exactly 3 rows resolve per poll, got {touched}");
    assert_eq!(
        app.resume_picker.resolved.len(),
        3,
        "resolved set tracks the 3 touched rows"
    );
    // A second poll resolves the remaining 2 (under the 3-cap).
    app.poll_agent();
    let touched2 = app
        .resume_picker
        .rows
        .iter()
        .filter(|r| r.title.starts_with("resolved-"))
        .count();
    assert_eq!(touched2, 5, "all 5 resolve after a second poll");
    assert_eq!(app.resume_picker.resolved.len(), 5);
    // A third poll resolves nothing new (all already resolved) — the
    // resolved set guards re-resolution so a row is never stat/read twice.
    app.poll_agent();
    assert_eq!(app.resume_picker.resolved.len(), 5);
}

/// A closed picker never resolves rows even with a lister wired (the open
/// guard short-circuits the whole block). Guards a regression where the
/// per-frame resolver ran unconditionally and burned I/O on every idle tick.
#[test]
fn test_poll_skips_resolution_closed() {
    let mut app = composition::app();
    app.session_lister = Some(Arc::new(ResolvingLister(1)));
    // Rows loaded but picker NOT open (e.g. closed by Esc, rows not cleared).
    app.resume_picker.rows = app.session_lister.clone().unwrap().list_sessions("");
    assert!(!app.resume_picker.open);
    app.poll_agent();
    assert!(
        !app.resume_picker.rows[0].title.starts_with("resolved-"),
        "closed picker must not resolve rows"
    );
    assert!(
        app.resume_picker.resolved.is_empty(),
        "resolved set stays empty when picker is closed"
    );
}

/// Lazy slug-dedup: when resolve_detail fills real titles, an older row
/// whose title duplicates a newer (already-resolved) row is hidden — the
/// picker shows one row per title, newest wins. Rows are sorted newest-first
/// and resolved top-to-bottom, so the first occurrence of each title stays
/// visible and later duplicates hide as they resolve. Restores the
/// pre-progressive same-title dedup under lazy loading.
#[test]
fn test_poll_dedups_dup_titles() {
    /// Two unnamed sessions whose resolve_detail yields the SAME title
    /// ("dup-title") — equivalent to re-running the same first prompt. The
    /// newer row (index 0, sorted first) stays; the older row (index 1)
    /// hides after it resolves.
    struct DupLister;
    impl SessionLister for DupLister {
        fn list_sessions(&self, _current: &str) -> Vec<SessionRow> {
            vec![
                SessionRow {
                    sid_str: "newer".into(),
                    title: "(session) aaaa".into(), // placeholder
                    cwd_basename: "repo".into(),
                    last_active: 200,
                    ..Default::default()
                },
                SessionRow {
                    sid_str: "older".into(),
                    title: "(session) bbbb".into(), // placeholder
                    cwd_basename: "repo".into(),
                    last_active: 100,
                    ..Default::default()
                },
            ]
        }
        fn resolve_detail(&self, row: &mut SessionRow) {
            // Both resolve to the same title (same first prompt).
            row.title = "dup-title".into();
        }
    }

    let mut app = composition::app();
    app.session_lister = Some(Arc::new(DupLister));
    app.resume_picker.rows = app.session_lister.clone().unwrap().list_sessions("");
    app.resume_picker.open();
    app.pane = Pane::Resume;
    // Both rows resolve in a single poll (2 rows < 3/frame cap). The newer
    // (index 0) resolves first -> seen_titles gets "dup-title". The older
    // (index 1) resolves second -> title already seen -> hidden.
    app.poll_agent();
    assert!(
        !app.resume_picker.rows[0].hidden,
        "newer row (first of the dup) stays visible"
    );
    assert!(
        app.resume_picker.rows[1].hidden,
        "older duplicate row must be hidden, got hidden={}",
        app.resume_picker.rows[1].hidden
    );
    // The filtered list the render uses excludes the hidden row.
    assert_eq!(
        app.resume_picker.filtered().len(),
        1,
        "picker shows one row per title after dedup"
    );
    assert_eq!(
        app.resume_picker.filtered()[0].sid_str,
        "newer",
        "the newest of the duplicates is the one kept"
    );
}
