//! Resume-picker interaction tests: the /resume command opens the picker
//! with a wired SessionLister, the direct-switch by name, + the filter.
//! Split out of interact_tests on size grounds. Uses the same render +
//! working helpers (a fresh App with a stub lister wired).

#![cfg(test)]

use houyicoder_protocol::frontend::SlashCommand;

use crate::composition;
use crate::state::Screen;
use crate::test_support::render_text;

fn working() -> crate::state::App {
    let mut app = composition::app();
    app.screen = Screen::Working;
    app
}

fn render(app: &crate::state::App) -> String {
    render_text(app, 100, 28)
}

/// A stub SessionLister: returns canned rows so the picker state + render +
/// /resume switch can be exercised without a real disk store (the real
/// lister is the CLI bridge, covered by a bin unit test + a PTY test).
struct StubLister(Vec<crate::resume_picker::SessionRow>);

impl crate::resume_picker::SessionLister for StubLister {
    fn list_sessions(&self, _current_sid: &str) -> Vec<crate::resume_picker::SessionRow> {
        self.0.clone()
    }

    // The stub rows already carry full titles + last_active (canned data), so
    // progressive detail resolution is a no-op here. The real bridge's
    // resolve_detail reads the log head + mtime; that path is covered by the
    // bridge's own tests, not here.
    fn resolve_detail(&self, _row: &mut crate::resume_picker::SessionRow) {}
}

fn stub_lister_app() -> crate::state::App {
    use crate::resume_picker::SessionRow;
    let mut app = working();
    app.session_lister = Some(std::sync::Arc::new(StubLister(vec![
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

/// /resume (no arg) with a lister opens the picker + the render shows the
/// rows (title + cwd basename, no sid).
#[test]
fn test_resume_opens_picker_lister() {
    let mut app = stub_lister_app();
    app.run_command(SlashCommand::Resume);
    assert!(app.resume_picker.open, "picker must open with a lister");
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
    let mut app = stub_lister_app();
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
    let mut app = stub_lister_app();
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
    let mut app = stub_lister_app();
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
    let mut app = stub_lister_app();
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
    let mut app = stub_lister_app();
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
    let mut app = stub_lister_app();
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
    app.session_lister = Some(std::sync::Arc::new(StubLister(vec![])));
    app.run_command(SlashCommand::Resume);
    assert!(!app.resume_picker.open, "picker must not open on empty");
    assert!(app.pending_resume_target.is_none(), "no sid on empty");
    let out = render(&app);
    assert!(
        out.contains("no other sessions"),
        "empty list should report no other sessions:\n{out}"
    );
}
