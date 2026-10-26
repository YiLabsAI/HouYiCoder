//! Progressive resume-resolution tests for poll_agent: the per-frame
//! resolver fills in picker row titles + last-active lazily, a few rows per
//! frame, so opening /resume never waits on the expensive per-row reads.
use crate::composition;
use crate::resume_picker::{SessionCatalog, SessionRow};
use crate::state::Pane;
use std::sync::Arc;

/// A catalog whose resolve_detail prepends "resolved-" to the title so the
/// test can count touched rows. sessions returns N unresolved rows.
struct ResolvingCatalog(usize);
impl SessionCatalog for ResolvingCatalog {
    fn sessions(&self, _current: &str) -> Vec<SessionRow> {
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
/// picker rows per frame (so opening the picker does not wait on the
/// expensive reads and fills in real titles + last-active top-to-bottom over
/// a few frames). The picker must be open for the branch to fire; a closed
/// picker resolves nothing.
#[test]
fn test_poll_resolves_rows_progressively() {
    let mut app = composition::app();
    app.sessions.catalog = Some(Arc::new(ResolvingCatalog(5)));
    // Open the picker + load rows (same path as /resume with no arg).
    app.ui.navigation.resume_picker.rows = app.sessions.catalog.clone().unwrap().sessions("");
    app.ui.navigation.resume_picker.open();
    app.ui.navigation.pane = Pane::Resume;
    assert!(app.ui.navigation.resume_picker.resolved.is_empty());
    // One poll resolves at most 3 rows (the per-frame cap).
    app.poll_agent();
    let touched = app
        .ui
        .navigation
        .resume_picker
        .rows
        .iter()
        .filter(|r| r.title.starts_with("resolved-"))
        .count();
    assert_eq!(touched, 3, "exactly 3 rows resolve per poll, got {touched}");
    assert_eq!(
        app.ui.navigation.resume_picker.resolved.len(),
        3,
        "resolved set tracks the 3 touched rows"
    );
    // A second poll resolves the remaining 2 (under the 3-cap).
    app.poll_agent();
    let touched2 = app
        .ui
        .navigation
        .resume_picker
        .rows
        .iter()
        .filter(|r| r.title.starts_with("resolved-"))
        .count();
    assert_eq!(touched2, 5, "all 5 resolve after a second poll");
    assert_eq!(app.ui.navigation.resume_picker.resolved.len(), 5);
    // A third poll resolves nothing new (all already resolved) — the
    // resolved set guards re-resolution so a row is never stat/read twice.
    app.poll_agent();
    assert_eq!(app.ui.navigation.resume_picker.resolved.len(), 5);
}

/// A closed picker never resolves rows even with a catalog wired (the open
/// guard short-circuits the whole block). Guards a regression where the
/// per-frame resolver ran unconditionally and burned I/O on every idle tick.
#[test]
fn test_poll_skips_resolution_closed() {
    let mut app = composition::app();
    app.sessions.catalog = Some(Arc::new(ResolvingCatalog(1)));
    // Rows loaded but picker NOT open (e.g. closed by Esc, rows not cleared).
    app.ui.navigation.resume_picker.rows = app.sessions.catalog.clone().unwrap().sessions("");
    assert!(!app.ui.navigation.resume_picker.open);
    app.poll_agent();
    assert!(
        !app.ui.navigation.resume_picker.rows[0]
            .title
            .starts_with("resolved-"),
        "closed picker must not resolve rows"
    );
    assert!(
        app.ui.navigation.resume_picker.resolved.is_empty(),
        "resolved set stays empty when picker is closed"
    );
}

/// Progressive resolution renames rows in place, and the selection stays on
/// the session the user picked: a row is chosen by identity, so a title
/// arriving a few frames later must not move the choice to another session.
#[test]
fn test_late_match_keeps_selection() {
    /// An earlier row that gains its title late, so it joins the filtered
    /// list above the cursor after the user has already chosen.
    struct LateMatchCatalog;
    impl SessionCatalog for LateMatchCatalog {
        fn sessions(&self, _current: &str) -> Vec<SessionRow> {
            vec![
                SessionRow {
                    sid_str: "sid-late".into(),
                    title: "(session) 0000".into(),
                    cwd_basename: "repo".into(),
                    ..Default::default()
                },
                SessionRow {
                    sid_str: "sid-target".into(),
                    title: "target service".into(),
                    cwd_basename: "repo".into(),
                    last_active: 1,
                    ..Default::default()
                },
            ]
        }
        fn resolve_detail(&self, row: &mut SessionRow) {
            if row.sid_str == "sid-late" {
                row.title = "target rollout".into();
            }
        }
    }

    let mut app = composition::app();
    app.sessions.catalog = Some(Arc::new(LateMatchCatalog));
    app.ui.navigation.resume_picker.rows = app.sessions.catalog.clone().unwrap().sessions("");
    app.ui.navigation.resume_picker.open();
    app.ui.navigation.pane = Pane::Resume;
    for letter in "target".chars() {
        app.ui.navigation.resume_picker.push(letter);
    }
    assert_eq!(
        app.ui
            .navigation
            .resume_picker
            .selected()
            .expect("one row matches before resolution")
            .sid_str,
        "sid-target"
    );
    app.poll_agent();
    let picker = &app.ui.navigation.resume_picker;
    assert_eq!(picker.len(), 2, "the late title joins the match set");
    assert_eq!(
        picker
            .selected()
            .expect("selection survives resolution")
            .sid_str,
        "sid-target",
        "a match appearing above the cursor must not move the choice"
    );
}

/// Two rows that resolve to the same title both stay in the list: identical
/// prompts are a normal outcome of re-running the same work, and the time
/// and project columns are what tell the two sessions apart.
#[test]
fn test_poll_keeps_duplicate_rows() {
    /// Two unnamed sessions whose resolve_detail yields the same title, as
    /// two runs of the same opening request would.
    struct DupCatalog;
    impl SessionCatalog for DupCatalog {
        fn sessions(&self, _current: &str) -> Vec<SessionRow> {
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
    app.sessions.catalog = Some(Arc::new(DupCatalog));
    app.ui.navigation.resume_picker.rows = app.sessions.catalog.clone().unwrap().sessions("");
    app.ui.navigation.resume_picker.open();
    app.ui.navigation.pane = Pane::Resume;
    // Both rows resolve in a single poll (2 rows < 3/frame cap).
    app.poll_agent();
    assert_eq!(
        app.ui.navigation.resume_picker.filtered().len(),
        2,
        "both duplicates stay listed"
    );
    assert_eq!(
        app.ui.navigation.resume_picker.rows[1].title, "dup-title",
        "the older row keeps the title it resolved"
    );
}
