use super::*;
use crate::composition::app;
use crate::input::queue::InputItem;
use crate::input::queue::resume_command_target;
use crate::records::TranscriptLine;
use crate::resume_picker::SessionCatalog;
use crate::resume_picker::SessionRow;
use crate::state::Pane;
use crate::state::Screen;
use crate::test_harness::connected_app;
use std::env::temp_dir;
use std::fs::create_dir_all;
use std::fs::remove_dir_all;
use std::fs::write;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// The resolved target carried by a queued /resume Command at the head.
fn queued_target(app: &App) -> Option<String> {
    app.ui
        .input
        .queue
        .first()
        .and_then(resume_command_target)
        .map(str::to_string)
}

/// The most recent system line pushed to the transcript, if any.
fn last_line(app: &App) -> Option<String> {
    app.sessions
        .current
        .transcript
        .iter()
        .rev()
        .find_map(|l| match l {
            TranscriptLine::System(s) => Some(s.clone()),
            _ => None,
        })
}

struct EmptyCatalog;
impl SessionCatalog for EmptyCatalog {
    fn sessions(&self, _current: &str) -> Vec<SessionRow> {
        Vec::new()
    }
}

struct TwoRowCatalog;
impl SessionCatalog for TwoRowCatalog {
    fn sessions(&self, _current: &str) -> Vec<SessionRow> {
        vec![
            SessionRow {
                sid_str: "aaaa1111".into(),
                title: "fix login".into(),
                cwd_basename: "app".into(),
                last_active: 1,
                ..Default::default()
            },
            SessionRow {
                sid_str: "bbbb2222".into(),
                title: "port tui".into(),
                cwd_basename: "app".into(),
                last_active: 2,
                ..Default::default()
            },
        ]
    }
}

/// A disconnected app reports not connected and never opens a pane.
#[test]
fn test_resume_disconnected() {
    let mut app = app();
    app.run_resume(None);
    assert!(app.ui.navigation.resume_picker.rows.is_empty());
    assert!(!app.ui.navigation.resume_picker.open);
    assert!(queued_target(&app).is_none());
    assert!(
        last_line(&app)
            .expect("a system line lands")
            .contains("resume: not connected"),
        "disconnected /resume reports not connected"
    );
}

/// A connected app without the session catalog reports the capability
/// gap, not a disconnect.
#[test]
fn test_resume_catalog_gap() {
    let mut app = connected_app();
    app.run_resume(None);
    assert!(!app.ui.navigation.resume_picker.open);
    assert!(
        last_line(&app)
            .expect("a system line lands")
            .contains("resume: session history unavailable"),
        "connected /resume without the catalog reports the capability gap"
    );
}

/// /resume with no arg and no other sessions reports empty + no pane.
#[test]
fn test_empty_store_reports_none() {
    let mut app = connected_app();
    app.sessions.catalog = Some(Arc::new(EmptyCatalog));
    app.run_resume(None);
    assert!(!app.ui.navigation.resume_picker.open);
    assert!(queued_target(&app).is_none());
}

/// /resume with no arg and sessions opens the Resume pane.
#[test]
fn test_opens_pane_with_rows() {
    let mut app = connected_app();
    app.sessions.catalog = Some(Arc::new(TwoRowCatalog));
    app.run_resume(None);
    assert!(app.ui.navigation.resume_picker.open);
    assert_eq!(app.ui.navigation.resume_picker.rows.len(), 2);
    assert_eq!(app.ui.navigation.pane, Pane::Resume);
}

/// Opening the picker loads the listing in one catalog call: the rows carry
/// their titles, so no per-row read follows the key that opened it.
#[test]
fn test_open_loads_rows_once() {
    struct CountingCatalog(AtomicUsize);
    impl SessionCatalog for CountingCatalog {
        fn sessions(&self, _current: &str) -> Vec<SessionRow> {
            self.0.fetch_add(1, Ordering::Relaxed);
            (0..3)
                .map(|i| SessionRow {
                    sid_str: format!("sid{i}"),
                    title: format!("(session) 000{i}"),
                    cwd_basename: "app".into(),
                    ..Default::default()
                })
                .collect()
        }
    }

    let catalog = Arc::new(CountingCatalog(AtomicUsize::new(0)));
    let mut app = connected_app();
    app.sessions.catalog = Some(catalog.clone());
    app.run_resume(None);
    assert!(app.ui.navigation.resume_picker.open);
    assert_eq!(
        catalog.0.load(Ordering::Relaxed),
        1,
        "one listing call opens the picker"
    );
}

/// /resume with a sid arg matches a row by sid and enqueues the switch.
#[test]
fn test_matches_sid_directly() {
    let mut app = connected_app();
    app.sessions.catalog = Some(Arc::new(TwoRowCatalog));
    app.run_resume(Some("aaaa1111"));
    assert_eq!(queued_target(&app).as_deref(), Some("aaaa1111"));
    assert!(!app.ui.quit, "in-process swap does not quit");
}

/// /resume with a title substring matches case-insensitively.
#[test]
fn test_matches_title_substring() {
    let mut app = connected_app();
    app.sessions.catalog = Some(Arc::new(TwoRowCatalog));
    app.run_resume(Some("LOGIN"));
    assert_eq!(queued_target(&app).as_deref(), Some("aaaa1111"));
}

/// /resume with a nonexistent token reports no match and does not quit.
#[test]
fn test_no_match_reports_error() {
    let mut app = connected_app();
    app.sessions.catalog = Some(Arc::new(TwoRowCatalog));
    app.run_resume(Some("zzz"));
    assert!(queued_target(&app).is_none());
    assert!(!app.ui.quit);
}

/// A mid-run /resume <sid> defers: it is enqueued as a Command (not
/// executed now), drained FIFO at idle so it does not fight the run's
/// writes. The swap does NOT happen at enqueue -- the drain re-dispatches
/// the command at idle, and try_switch_session does the swap. A bare
/// /resume (no arg) still opens the picker (read-only browsing).
#[test]
fn test_busy_run_defers_resume() {
    let mut app = app();
    app.sessions.catalog = Some(Arc::new(TwoRowCatalog));
    app.start_run_for_test(0);
    app.ui.navigation.screen = Screen::Working;
    // /resume <sid> mid-run -> enqueued as a Command, not executed.
    app.ui.input.editor.set("/resume aaaa1111".to_string());
    app.submit_input();
    assert_eq!(
        app.ui.input.queue,
        vec![InputItem::Command("/resume aaaa1111".into())],
        "/resume <sid> mid-run enqueued as a Command"
    );
    assert_eq!(
        app.sessions.current.identity.session_id.0, "test",
        "mid-run /resume defers the swap (no switch at enqueue)"
    );
    assert!(app.agent_busy(), "the run is still in progress");
}

/// A bare /resume (no arg) mid-run does NOT defer -- it opens the picker
/// (read-only browsing is harmless). Only a specific target defers.
#[test]
fn test_busy_bare_opens_picker() {
    let mut app = connected_app();
    app.sessions.catalog = Some(Arc::new(TwoRowCatalog));
    app.start_run_for_test(0);
    app.ui.navigation.screen = Screen::Working;
    app.ui.input.editor.set("/resume".to_string());
    app.submit_input();
    assert!(
        app.ui.input.queue.is_empty(),
        "bare /resume does not enqueue (picker opens instead)"
    );
    assert!(
        app.ui.navigation.resume_picker.open,
        "picker opens for browsing"
    );
}

/// A non-existent .json path is NOT treated as an export file.
#[test]
fn test_missing_json_not_export() {
    assert!(!is_export_file_path("./nonexistent-export.json"));
    assert!(!is_export_file_path("abc123"));
}

/// An existing .json path is an export resume: a /resume Command with that
/// path is enqueued at the head, so the event loop's idle drain swaps the
/// session in-process (or hands the target up on loop exit).
#[test]
fn test_export_path_enqueues_resume() {
    let dir = temp_dir().join(format!(
        "houyi-resume-export-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    create_dir_all(&dir).expect("temp dir");
    let path = dir.join("transcript.json");
    write(&path, "[]").expect("write export file");
    let mut app = connected_app();
    app.sessions.catalog = Some(Arc::new(EmptyCatalog));
    let arg = path.to_str().expect("utf8 path").to_string();
    app.run_resume(Some(&arg));
    assert_eq!(
        queued_target(&app).as_deref(),
        Some(arg.as_str()),
        "the export path rides a /resume Command at the head"
    );
    assert!(!app.ui.quit, "in-process swap does not quit");
    drop(remove_dir_all(&dir));
}

/// A catalog of named sessions.
struct ManyRowCatalog(usize);
impl SessionCatalog for ManyRowCatalog {
    fn sessions(&self, _current: &str) -> Vec<SessionRow> {
        (0..self.0)
            .map(|i| SessionRow {
                sid_str: format!("sid{i:04}"),
                title: format!("named {i}"),
                cwd_basename: "app".into(),
                last_active: i as u64,
                ..Default::default()
            })
            .collect()
    }
}

/// A store with many sessions lists every one of them: a title that counts
/// as its own duplicate would drop rows, so this pins that a named session
/// keeps its own name.
#[test]
fn test_many_sessions_all_visible() {
    let mut app = connected_app();
    app.sessions.catalog = Some(Arc::new(ManyRowCatalog(25)));
    app.run_resume(None);
    assert_eq!(app.ui.navigation.resume_picker.rows.len(), 25);
    assert_eq!(
        app.ui.navigation.resume_picker.filtered().len(),
        25,
        "every named session stays listed"
    );
}

/// A catalog where the first row is unavailable to this process.
struct BusyCatalog;
impl SessionCatalog for BusyCatalog {
    fn sessions(&self, _current: &str) -> Vec<SessionRow> {
        vec![
            SessionRow {
                sid_str: "held1111".into(),
                title: "busy elsewhere".into(),
                cwd_basename: "app".into(),
                last_active: 2,
                unavailable: true,
                ..Default::default()
            },
            SessionRow {
                sid_str: "free2222".into(),
                title: "open the repo".into(),
                cwd_basename: "app".into(),
                last_active: 1,
                ..Default::default()
            },
        ]
    }
}

/// The picker leaves out a session this process cannot open and counts it in
/// the pane state, so the row's absence is stated where the listing is. The
/// free row is still offered and opening the pane writes no transcript line.
#[test]
fn test_picker_skips_unavailable() {
    let mut app = connected_app();
    app.sessions.catalog = Some(Arc::new(BusyCatalog));
    app.run_resume(None);
    assert!(app.ui.navigation.resume_picker.open);
    let rows = &app.ui.navigation.resume_picker.rows;
    assert_eq!(rows.len(), 1, "the unavailable row leaves the listing");
    assert_eq!(rows[0].sid_str, "free2222");
    assert_eq!(app.ui.navigation.resume_picker.hidden, 1);
}

/// Every left-out session is counted, not just the first, because the pane
/// header reports the total.
#[test]
fn test_picker_counts_hidden() {
    struct TwoBusy;
    impl SessionCatalog for TwoBusy {
        fn sessions(&self, _current: &str) -> Vec<SessionRow> {
            vec![
                SessionRow {
                    sid_str: "held1111".into(),
                    unavailable: true,
                    ..Default::default()
                },
                SessionRow {
                    sid_str: "held2222".into(),
                    unavailable: true,
                    ..Default::default()
                },
                SessionRow {
                    sid_str: "free3333".into(),
                    ..Default::default()
                },
            ]
        }
    }
    let mut app = connected_app();
    app.sessions.catalog = Some(Arc::new(TwoBusy));
    app.run_resume(None);
    assert_eq!(app.ui.navigation.resume_picker.hidden, 2);
}

/// An explicit lookup by name reaches an unavailable row: the switch then
/// reports the real refusal with its options, instead of claiming no session
/// matches.
#[test]
fn test_lookup_finds_unavailable() {
    let mut app = connected_app();
    app.sessions.catalog = Some(Arc::new(BusyCatalog));
    app.run_resume(Some("busy elsewhere"));
    assert_eq!(queued_target(&app).as_deref(), Some("held1111"));
}

/// When every other session is unavailable the picker does not open, and
/// the line names the reason and the way to reach the refusal for one of
/// them.
#[test]
fn test_all_unavailable_names_reason() {
    struct AllBusy;
    impl SessionCatalog for AllBusy {
        fn sessions(&self, _current: &str) -> Vec<SessionRow> {
            vec![SessionRow {
                sid_str: "held1111".into(),
                title: "busy elsewhere".into(),
                unavailable: true,
                ..Default::default()
            }]
        }
    }
    let mut app = connected_app();
    app.sessions.catalog = Some(Arc::new(AllBusy));
    app.run_resume(None);
    assert!(!app.ui.navigation.resume_picker.open);
    assert!(
        last_line(&app)
            .expect("a line lands")
            .contains("no other recent session is available from this process"),
        "the empty state names the blockers, not the disk"
    );
}

/// The picker keeps at most the visible limit, so a large store never hands
/// the pane more rows than it renders.
#[test]
fn test_picker_caps_rows() {
    let mut app = connected_app();
    app.sessions.catalog = Some(Arc::new(ManyRowCatalog(VISIBLE_LIMIT + 40)));
    app.run_resume(None);
    assert_eq!(app.ui.navigation.resume_picker.rows.len(), VISIBLE_LIMIT);
}

/// The listing is complete after skipping unavailable rows, because the
/// skip happens before the cap. Capping first would let an unavailable row
/// consume a slot, leaving the pane a row short.
#[test]
fn test_picker_keeps_full_limit() {
    struct TwoBusyCatalog(usize);
    impl SessionCatalog for TwoBusyCatalog {
        fn sessions(&self, _current: &str) -> Vec<SessionRow> {
            (0..self.0)
                .map(|i| SessionRow {
                    sid_str: format!("sid{i:04}"),
                    title: format!("named {i}"),
                    unavailable: i == 7 || i == 57,
                    ..Default::default()
                })
                .collect()
        }
    }
    let mut app = connected_app();
    app.sessions.catalog = Some(Arc::new(TwoBusyCatalog(VISIBLE_LIMIT + 40)));
    app.run_resume(None);
    let rows = &app.ui.navigation.resume_picker.rows;
    assert_eq!(
        rows.len(),
        VISIBLE_LIMIT,
        "free rows fill the limit after the unavailable ones leave"
    );
    assert!(
        rows.iter().all(|row| !row.unavailable),
        "no unavailable row survives the skip"
    );
}
