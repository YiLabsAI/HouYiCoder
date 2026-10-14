use super::*;
use crate::composition::app;
use crate::input::queue::InputItem;
use crate::input::queue::resume_command_target;
use crate::resume_picker::SessionCatalog;
use crate::resume_picker::SessionRow;
use crate::state::Screen;
use crate::test_harness::connected_app;
use std::sync::Arc;

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
            crate::records::TranscriptLine::System(s) => Some(s.clone()),
            _ => None,
        })
}

struct EmptyCatalog;
impl SessionCatalog for EmptyCatalog {
    fn sessions(&self, _current: &str) -> Vec<SessionRow> {
        Vec::new()
    }
    fn resolve_detail(&self, _row: &mut SessionRow) {}
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
    fn resolve_detail(&self, _row: &mut SessionRow) {}
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
    assert_eq!(app.ui.navigation.pane, crate::state::Pane::Resume);
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
    let dir = std::env::temp_dir().join(format!(
        "houyi-resume-export-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("transcript.json");
    std::fs::write(&path, "[]").expect("write export file");
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
    drop(std::fs::remove_dir_all(&dir));
}

/// A catalog of named sessions, so resolve_detail leaves every title as
/// the row already carries it.
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
    fn resolve_detail(&self, _row: &mut SessionRow) {}
}

/// A store with more sessions than the picker resolves on open: the rows
/// past that first batch resolve in the poll loop and must stay listed.
/// A title that counts as its own duplicate would drop them, so this
/// pins that a named session keeps its own name.
#[test]
fn test_many_sessions_all_visible() {
    let mut app = connected_app();
    app.sessions.catalog = Some(Arc::new(ManyRowCatalog(25)));
    app.run_resume(None);
    assert_eq!(app.ui.navigation.resume_picker.rows.len(), 25);
    assert_eq!(
        app.ui.navigation.resume_picker.filtered().len(),
        25,
        "at open"
    );
    for _ in 0..4 {
        app.poll_agent();
    }
    assert_eq!(
        app.ui.navigation.resume_picker.filtered().len(),
        25,
        "every named session stays listed after the poll loop resolves the rest"
    );
}
