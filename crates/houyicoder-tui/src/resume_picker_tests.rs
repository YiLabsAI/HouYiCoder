//! Resume picker opening, filtering, and direct session switching tests.

#![cfg(test)]

use houyicoder_protocol::frontend::SlashCommand;

use crate::resume_picker::{SessionCatalog, SessionPickerState, SessionRow};
use crate::test_harness::render_text;

fn working() -> crate::state::App {
    // Picker flows drive run_resume, which needs a live session (the
    // disconnected branch refuses before the catalog logic).
    crate::test_harness::connected_app()
}

fn render(app: &crate::state::App) -> String {
    render_text(app, 100, 28)
}

/// A stub SessionCatalog: returns canned rows so the picker state + render +
/// /resume switch can be exercised without a real disk store (the real
/// catalog is the CLI implementation, covered by a bin unit test + a PTY test).
struct StubCatalog(Vec<SessionRow>);

impl SessionCatalog for StubCatalog {
    fn sessions(&self, _current_sid: &str) -> Vec<SessionRow> {
        self.0.clone()
    }

    // The stub rows already carry full titles + last_active (canned data), so
    // progressive detail resolution is a no-op here. The real catalog's
    // resolve_detail reads the log head + mtime; that path is covered by the
    // catalog's own tests, not here.
    fn resolve_detail(&self, _row: &mut SessionRow) {}
}

fn stub_catalog_app() -> crate::state::App {
    let mut app = working();
    app.session_catalog = Some(std::sync::Arc::new(StubCatalog(vec![
        SessionRow {
            sid_str: "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".into(),
            title: "login flow rework".into(),
            cwd_basename: "hicoder".into(),
            last_active: 1000,
            ..Default::default()
        },
        SessionRow {
            sid_str: "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb".into(),
            title: "search pane".into(),
            cwd_basename: "demo-app".into(),
            last_active: 2000,
            ..Default::default()
        },
    ])));
    app
}

/// /resume (no arg) with a catalog opens the picker + the render shows the
/// rows (title + cwd basename, no sid).
#[test]
fn test_resume_opens_picker_catalog() {
    let mut app = stub_catalog_app();
    app.run_command(SlashCommand::Resume);
    assert!(app.resume_picker.open, "picker must open with a catalog");
    let out = render(&app);
    println!("--- /resume picker ---\n{out}\n--- end ---");
    assert!(
        out.contains("Resume a session"),
        "picker header missing:\n{out}"
    );
    assert!(
        out.contains("login flow rework"),
        "row title missing:\n{out}"
    );
    assert!(
        out.contains("search pane"),
        "second row title missing:\n{out}"
    );
    assert!(
        out.contains("hicoder") && out.contains("demo-app"),
        "cwd basenames missing:\n{out}"
    );
    assert!(
        !out.contains("aaaaaaaa-aaaa"),
        "sid must not be shown in the picker:\n{out}"
    );
}

/// /resume <name> switches directly (sets pending_resume_target, no quit —
/// the event loop swaps the session in-process via resume_builder).
#[test]
fn test_resume_name_switches_directly() {
    let mut app = stub_catalog_app();
    app.run_tui_local_command("resume login flow");
    assert!(
        !app.resume_picker.open,
        "direct switch must not open picker"
    );
    assert_eq!(
        app.pending_resume_target.as_deref(),
        Some("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"),
        "pending sid must be the matched row sid"
    );
    assert!(!app.quit, "in-process swap does not quit");
}

/// Typing in the open picker narrows the list by sid OR title.
#[test]
fn test_resume_picker_filters_query() {
    let mut app = stub_catalog_app();
    app.run_command(SlashCommand::Resume);
    app.resume_picker.push('s');
    app.resume_picker.push('e');
    app.resume_picker.push('a');
    assert_eq!(app.resume_picker.len(), 1, "query narrows to one row");
    let out = render(&app);
    assert!(out.contains("search pane"), "filtered row shown:\n{out}");
    assert!(
        !out.contains("login flow"),
        "non-matching row hidden:\n{out}"
    );
}

/// Picker keys: Up/Down move the selection, typing narrows, Enter resumes
/// (sets pending_resume_target, no quit — in-process swap), Esc closes.
/// Drives the real handle_key path so the key dispatch (not just the state
/// methods) is covered.
#[test]
fn test_keys_navigate_select_close() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut app = stub_catalog_app();
    app.run_command(SlashCommand::Resume);
    assert!(app.resume_picker.open);
    crate::app::handle_key(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.resume_picker.sel, 1, "Down moves to row 1");
    crate::app::handle_key(&mut app, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.resume_picker.sel, 0, "Up wraps back to row 0");
    crate::app::handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        app.pending_resume_target.as_deref(),
        Some("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"),
        "Enter resumes the selected row"
    );
    assert!(!app.quit, "in-process swap does not quit");
    assert!(!app.resume_picker.open, "Enter closes the picker");
    app.pending_resume_target = None;
    app.run_command(SlashCommand::Resume);
    crate::app::handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!app.resume_picker.open, "Esc closes the picker");
    assert!(app.pending_resume_target.is_none(), "Esc does not resume");
}

/// Backspace on an empty query closes the picker; on a non-empty query it
/// pops the last char.
#[test]
fn test_picker_backspace_pops_closes() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut app = stub_catalog_app();
    app.run_command(SlashCommand::Resume);
    // Type then backspace: pops the char.
    app.resume_picker.push('s');
    crate::app::handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
    );
    assert!(
        app.resume_picker.open,
        "backspace on non-empty query stays open"
    );
    assert!(
        app.resume_picker.query.is_empty(),
        "backspace pops the char"
    );
    // Backspace on empty query closes.
    crate::app::handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
    );
    assert!(!app.resume_picker.open, "backspace on empty query closes");
}

/// Enter with a query that matches no row falls back to a direct switch by
/// the typed query (sid or name); no match yields a system line, not a
/// silent no-op. Covers the Enter fall-back branch.
#[test]
fn test_picker_enter_fall_back() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut app = stub_catalog_app();
    app.run_command(SlashCommand::Resume);
    // Type a query matching neither row's sid nor title.
    for c in "zzz-no-match".chars() {
        app.resume_picker.push(c);
    }
    assert!(app.resume_picker.selected().is_none(), "no row matches");
    crate::app::handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(!app.quit, "no match must not quit (no resume)");
    let out = render(&app);
    assert!(
        out.contains("no session matches"),
        "fall-back no-match should report:\n{out}"
    );
}

/// Char keys reach the picker via handle_key (not just the push method), so
/// the key dispatch's Char arm is exercised.
#[test]
fn test_char_keys_reach_push() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut app = stub_catalog_app();
    app.run_command(SlashCommand::Resume);
    crate::app::handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
    );
    assert_eq!(app.resume_picker.query, "l", "Char pushes to the query");
    // A non-graphic char (e.g. newline) is ignored (the _ arm).
    crate::app::handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('\n'), KeyModifiers::NONE),
    );
    assert_eq!(app.resume_picker.query, "l", "non-graphic char ignored");
}

/// /resume with no other sessions on disk shows a system line instead of
/// opening an empty picker (no crash, no empty overlay). Guards against a
/// regression where the picker opens with zero rows and Enter does nothing.
#[test]
fn test_resume_picker_empty_list() {
    let mut app = working();
    app.session_catalog = Some(std::sync::Arc::new(StubCatalog(vec![])));
    app.run_command(SlashCommand::Resume);
    assert!(!app.resume_picker.open, "picker must not open on empty");
    assert!(app.pending_resume_target.is_none(), "no sid on empty");
    let out = render(&app);
    assert!(
        out.contains("no other sessions"),
        "empty list should report no other sessions:\n{out}"
    );
}

/// A catalog whose detail resolution rewrites an unnamed row's placeholder to
/// the slug of its first prompt, so the lazy dedup has a collision to act on.
struct SlugCatalog;
impl SessionCatalog for SlugCatalog {
    fn sessions(&self, _current_sid: &str) -> Vec<SessionRow> {
        vec![
            SessionRow {
                sid_str: "newest".into(),
                title: "fix login".into(),
                last_active: 3000,
                ..Default::default()
            },
            SessionRow {
                sid_str: "older".into(),
                title: "(session) aaaaaaaa".into(),
                last_active: 2000,
                ..Default::default()
            },
            SessionRow {
                sid_str: "oldest".into(),
                title: "(session) bbbbbbbb".into(),
                last_active: 1000,
                ..Default::default()
            },
        ]
    }

    fn resolve_detail(&self, row: &mut SessionRow) {
        if row.sid_str == "older" {
            row.title = "fix login".into();
        } else if row.sid_str == "oldest" {
            row.title = "port tui".into();
        }
    }
}

/// The lazy dedup hides an older row whose resolved slug repeats a newer
/// row's title, and leaves the newer row and an unrelated slug listed.
#[test]
fn test_duplicate_title_hides_older() {
    let mut p = SessionPickerState {
        rows: SlugCatalog.sessions(""),
        ..Default::default()
    };
    p.open();
    p.resolve_rows(&SlugCatalog, 3);
    assert!(!p.rows[0].hidden, "the newest row keeps its title");
    assert!(p.rows[1].hidden, "the older row repeats the newest title");
    assert!(!p.rows[2].hidden, "an unrelated slug stays listed");
    assert_eq!(p.filtered().len(), 2, "the duplicate row is not listed");
}

/// The same collision through the real open path + the picker render: the
/// duplicated title reaches the screen once, so the hidden row is not drawn.
#[test]
fn test_duplicate_title_renders_once() {
    let mut app = working();
    app.session_catalog = Some(std::sync::Arc::new(SlugCatalog));
    app.run_command(SlashCommand::Resume);
    let out = render(&app);
    assert_eq!(
        out.matches("fix login").count(),
        1,
        "the duplicated title is drawn once:\n{out}"
    );
    assert!(
        out.contains("port tui"),
        "the unrelated slug is drawn:\n{out}"
    );
}

/// A catalog where the collision runs the other way: the newer row is the
/// unnamed one and the older row already carries the name.
struct NamedBelowCatalog;
impl SessionCatalog for NamedBelowCatalog {
    fn sessions(&self, _current_sid: &str) -> Vec<SessionRow> {
        vec![
            SessionRow {
                sid_str: "newer-unnamed".into(),
                title: "(session) cccccccc".into(),
                last_active: 3000,
                ..Default::default()
            },
            SessionRow {
                sid_str: "older-named".into(),
                title: "fix login".into(),
                last_active: 2000,
                ..Default::default()
            },
        ]
    }

    fn resolve_detail(&self, row: &mut SessionRow) {
        if row.sid_str == "newer-unnamed" {
            row.title = "fix login".into();
        }
    }
}

/// The title goes to the row resolved first, not to the row carrying a
/// descriptor name, so an older named row yields to a newer row whose slug
/// matches. Pins the precedence the picker shares with the catalog's own
/// cheap-title dedup.
#[test]
fn test_older_named_row_hides() {
    let mut p = SessionPickerState {
        rows: NamedBelowCatalog.sessions(""),
        ..Default::default()
    };
    p.open();
    p.resolve_rows(&NamedBelowCatalog, 2);
    assert!(!p.rows[0].hidden, "the newer row claims the title");
    assert!(p.rows[1].hidden, "the older named row is the duplicate");
    assert_eq!(p.filtered().len(), 1, "only the claiming row is listed");
}
