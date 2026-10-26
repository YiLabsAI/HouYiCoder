//! Resume picker opening, filtering, and direct session switching tests.

#![cfg(test)]

use houyicoder_protocol::frontend::SlashCommand;

use crate::app::handle_key;
use crate::input::queue::InputItem;
use crate::input::queue::resume_command_target;
use crate::resume_picker::{SessionCatalog, SessionPickerState, SessionRow, TitleSource};
use crate::state::App;
use crate::test_harness::connected_app;
use crate::test_harness::render_text;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
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

fn working() -> App {
    // Picker flows drive run_resume, which needs a live session (the
    // disconnected branch refuses before the catalog logic).
    connected_app()
}

fn render(app: &App) -> String {
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

fn stub_catalog_app() -> App {
    let mut app = working();
    app.sessions.catalog = Some(Arc::new(StubCatalog(vec![
        SessionRow {
            sid_str: "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".into(),
            title: "login flow rework".into(),
            title_source: TitleSource::LastPrompt,
            cwd_basename: "hicoder".into(),
            first_prompt: Some("port the parser".into()),
            last_prompt: Some("login flow rework".into()),
            last_active: 1000,
            ..Default::default()
        },
        SessionRow {
            sid_str: "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb".into(),
            title: "search pane".into(),
            title_source: TitleSource::FirstPrompt,
            cwd_basename: "demo-app".into(),
            first_prompt: Some("search pane".into()),
            last_prompt: Some("close the loop".into()),
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
    assert!(
        app.ui.navigation.resume_picker.open,
        "picker must open with a catalog"
    );
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

/// /resume <name> switches directly (enqueues a /resume Command, no quit —
/// the event loop swaps the session in-process via resume_builder).
#[test]
fn test_resume_name_switches_directly() {
    let mut app = stub_catalog_app();
    app.run_tui_local_command("resume login flow");
    assert!(
        !app.ui.navigation.resume_picker.open,
        "direct switch must not open picker"
    );
    assert_eq!(
        queued_target(&app).as_deref(),
        Some("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"),
        "pending sid must be the matched row sid"
    );
    assert!(!app.ui.quit, "in-process swap does not quit");
}

/// Typing in the open picker narrows the list by sid OR title.
#[test]
fn test_resume_picker_filters_query() {
    let mut app = stub_catalog_app();
    app.run_command(SlashCommand::Resume);
    app.ui.navigation.resume_picker.push('s');
    app.ui.navigation.resume_picker.push('e');
    app.ui.navigation.resume_picker.push('a');
    assert_eq!(
        app.ui.navigation.resume_picker.len(),
        1,
        "query narrows to one row"
    );
    let out = render(&app);
    assert!(out.contains("search pane"), "filtered row shown:\n{out}");
    assert!(
        !out.contains("login flow"),
        "non-matching row hidden:\n{out}"
    );
}

/// Picker keys: Up/Down move the selection, typing narrows, Enter resumes
/// (enqueues a /resume Command, no quit — in-process swap), Esc closes.
/// Drives the real handle_key path so the key dispatch (not just the state
/// methods) is covered.
#[test]
fn test_keys_navigate_select_close() {
    let mut app = stub_catalog_app();
    app.run_command(SlashCommand::Resume);
    assert!(app.ui.navigation.resume_picker.open);
    handle_key(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(
        app.ui.navigation.resume_picker.sel, 1,
        "Down moves to row 1"
    );
    handle_key(&mut app, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(
        app.ui.navigation.resume_picker.sel, 0,
        "Up wraps back to row 0"
    );
    handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        queued_target(&app).as_deref(),
        Some("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"),
        "Enter resumes the selected row"
    );
    assert!(!app.ui.quit, "in-process swap does not quit");
    assert!(
        !app.ui.navigation.resume_picker.open,
        "Enter closes the picker"
    );
    drop(app.ui.input.queue.take_all());
    app.run_command(SlashCommand::Resume);
    handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(
        !app.ui.navigation.resume_picker.open,
        "Esc closes the picker"
    );
    assert!(queued_target(&app).is_none(), "Esc does not resume");
}

/// Backspace on an empty query closes the picker; on a non-empty query it
/// pops the last char.
#[test]
fn test_picker_backspace_pops_closes() {
    let mut app = stub_catalog_app();
    app.run_command(SlashCommand::Resume);
    // Type then backspace: pops the char.
    app.ui.navigation.resume_picker.push('s');
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
    );
    assert!(
        app.ui.navigation.resume_picker.open,
        "backspace on non-empty query stays open"
    );
    assert!(
        app.ui.navigation.resume_picker.query.is_empty(),
        "backspace pops the char"
    );
    // Backspace on empty query closes.
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
    );
    assert!(
        !app.ui.navigation.resume_picker.open,
        "backspace on empty query closes"
    );
}

/// Enter with a query that matches no row falls back to a direct switch by
/// the typed query (sid or name); no match yields a system line, not a
/// silent no-op. Covers the Enter fall-back branch.
#[test]
fn test_picker_enter_fall_back() {
    let mut app = stub_catalog_app();
    app.run_command(SlashCommand::Resume);
    // Type a query matching neither row's sid nor title.
    for c in "zzz-no-match".chars() {
        app.ui.navigation.resume_picker.push(c);
    }
    assert!(
        app.ui.navigation.resume_picker.selected().is_none(),
        "no row matches"
    );
    handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(!app.ui.quit, "no match must not quit (no resume)");
    let out = render(&app);
    assert!(
        out.contains("no session matches"),
        "fall-back no-match should report:\n{out}"
    );
}

/// Enter with a selected row while a run is in flight defers the switch onto
/// the queued-input FIFO instead of swapping now (a swap would fight the run).
#[test]
fn test_busy_defers_selection() {
    let mut app = stub_catalog_app();
    app.run_command(SlashCommand::Resume);
    app.start_run_for_test(1);
    let sid_before = app.sessions.current.identity.session_id.0.clone();
    handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        app.sessions.current.identity.session_id.0, sid_before,
        "busy Enter must not swap now"
    );
    assert_eq!(app.ui.input.queue.len(), 1, "the switch is deferred");
    match app.ui.input.queue.first() {
        Some(InputItem::Command(text)) => {
            assert_eq!(text, "/resume aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
        }
        other => panic!("expected a deferred /resume command, got {other:?}"),
    }
}

/// Enter with a typed query matching no row while busy defers the direct
/// switch by query onto the queue. Covers the no-match defer branch.
#[test]
fn test_busy_defers_query() {
    let mut app = stub_catalog_app();
    app.run_command(SlashCommand::Resume);
    for c in "zzz-no-match".chars() {
        app.ui.navigation.resume_picker.push(c);
    }
    app.start_run_for_test(1);
    let sid_before = app.sessions.current.identity.session_id.0.clone();
    handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        app.sessions.current.identity.session_id.0, sid_before,
        "busy Enter must not swap now"
    );
    assert_eq!(app.ui.input.queue.len(), 1, "the switch is deferred");
    match app.ui.input.queue.first() {
        Some(InputItem::Command(text)) => {
            assert_eq!(text, "/resume zzz-no-match");
        }
        other => panic!("expected a deferred /resume command, got {other:?}"),
    }
}

/// Char keys reach the picker via handle_key (not just the push method), so
/// the key dispatch's Char arm is exercised.
#[test]
fn test_char_keys_reach_push() {
    let mut app = stub_catalog_app();
    app.run_command(SlashCommand::Resume);
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
    );
    assert_eq!(
        app.ui.navigation.resume_picker.query, "l",
        "Char pushes to the query"
    );
    // A non-graphic char (e.g. newline) is ignored (the _ arm).
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('\n'), KeyModifiers::NONE),
    );
    assert_eq!(
        app.ui.navigation.resume_picker.query, "l",
        "non-graphic char ignored"
    );
}

/// Text outside ASCII still reaches the filter: sessions are named in the
/// user's own language, so the acceptance rule is "not a control character".
#[test]
fn test_typed_character_filters() {
    let mut app = stub_catalog_app();
    app.run_command(SlashCommand::Resume);
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('会'), KeyModifiers::NONE),
    );
    assert_eq!(
        app.ui.navigation.resume_picker.query, "会",
        "a wide character joins the filter"
    );
}

/// /resume with no other sessions on disk shows a system line instead of
/// opening an empty picker (no crash, no empty overlay). Guards against a
/// regression where the picker opens with zero rows and Enter does nothing.
#[test]
fn test_resume_picker_empty_list() {
    let mut app = working();
    app.sessions.catalog = Some(Arc::new(StubCatalog(vec![])));
    app.run_command(SlashCommand::Resume);
    assert!(
        !app.ui.navigation.resume_picker.open,
        "picker must not open on empty"
    );
    assert!(queued_target(&app).is_none(), "no sid on empty");
    let out = render(&app);
    assert!(
        out.contains("no other sessions"),
        "empty list should report no other sessions:\n{out}"
    );
}

/// A catalog whose detail resolution names an unnamed row, so two rows can
/// carry the same title after resolution.
struct SharedTitleCatalog;
impl SessionCatalog for SharedTitleCatalog {
    fn sessions(&self, _current_sid: &str) -> Vec<SessionRow> {
        vec![
            SessionRow {
                sid_str: "newest".into(),
                title: "fix login".into(),
                title_source: TitleSource::User,
                last_active: 3000,
                ..Default::default()
            },
            SessionRow {
                sid_str: "older".into(),
                title: "(session) aaaaaaaa".into(),
                last_active: 2000,
                ..Default::default()
            },
        ]
    }

    fn resolve_detail(&self, row: &mut SessionRow) {
        if row.sid_str == "older" {
            row.title = "fix login".into();
            row.title_source = TitleSource::FirstPrompt;
        }
    }
}

/// Two sessions can end up with the same title, and both stay listed: the
/// time and project columns are what tells them apart.
#[test]
fn test_duplicate_title_keeps_rows() {
    let mut p = SessionPickerState {
        rows: SharedTitleCatalog.sessions(""),
        ..Default::default()
    };
    p.open();
    p.resolve_rows(&SharedTitleCatalog, 2);
    assert_eq!(p.rows[1].title, "fix login", "the older row resolves");
    assert_eq!(p.filtered().len(), 2, "both rows are listed");
}

/// The same pair through the picker render: the shared title is drawn on
/// both rows rather than collapsing to one.
#[test]
fn test_duplicate_title_renders_both() {
    let mut app = working();
    app.sessions.catalog = Some(Arc::new(SharedTitleCatalog));
    app.run_command(SlashCommand::Resume);
    // Detail resolves from the poll loop, so the listing needs one frame
    // before the derived titles are on screen.
    app.poll_agent();
    let out = render(&app);
    assert_eq!(
        out.matches("fix login").count(),
        2,
        "both rows are drawn:\n{out}"
    );
}

/// A catalog whose rows carry more than the title: the first and newest
/// prompt each, and the project directory.
struct TopicCatalog;
impl SessionCatalog for TopicCatalog {
    fn sessions(&self, _current_sid: &str) -> Vec<SessionRow> {
        vec![
            SessionRow {
                sid_str: "aaaaaaaa".into(),
                title: "wire the table".into(),
                title_source: TitleSource::LastPrompt,
                cwd_basename: "hicoder".into(),
                first_prompt: Some("research the demo repo".into()),
                last_prompt: Some("wire the table".into()),
                last_active: 3000,
                ..Default::default()
            },
            SessionRow {
                sid_str: "bbbbbbbb".into(),
                title: "named run".into(),
                title_source: TitleSource::User,
                cwd_basename: "demo-app".into(),
                first_prompt: Some("port the parser".into()),
                last_prompt: Some("close the loop".into()),
                last_active: 2000,
                ..Default::default()
            },
        ]
    }

    fn resolve_detail(&self, _row: &mut SessionRow) {}
}

/// The selected row gains a second line saying what the work started as, and
/// moving the selection moves that line: the other row keeps one line so the
/// list stays scannable.
#[test]
fn test_selected_row_shows_topic() {
    let mut app = working();
    app.sessions.catalog = Some(Arc::new(TopicCatalog));
    app.run_command(SlashCommand::Resume);
    let out = render(&app);
    assert!(
        out.contains("topic: research the demo repo"),
        "the opening subject is shown under the selected row:\n{out}"
    );
    assert!(
        !out.contains("close the loop"),
        "an unselected row keeps one line:\n{out}"
    );
    handle_key(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    let out = render(&app);
    assert!(
        out.contains("topic: close the loop"),
        "a user-named row shows the newest prompt instead:\n{out}"
    );
    assert!(
        !out.contains("topic: research the demo repo"),
        "the previously selected row loses its topic line:\n{out}"
    );
}

/// The filter reaches every column the design names: the newest prompt, the
/// first prompt, the project directory, and the session id.
#[test]
fn test_filter_matches_record_fields() {
    let mut app = stub_catalog_app();
    app.run_command(SlashCommand::Resume);
    let filter_by = |app: &mut App, text: &str| {
        let picker = &mut app.ui.navigation.resume_picker;
        picker.query.clear();
        for c in text.chars() {
            picker.push(c);
        }
        picker
            .filtered()
            .iter()
            .map(|r| r.sid_str.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        filter_by(&mut app, "parser"),
        vec!["aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".to_string()],
        "a word only the first prompt holds still finds the session"
    );
    assert_eq!(
        filter_by(&mut app, "close the loop"),
        vec!["bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb".to_string()],
        "a word only the newest prompt holds still finds the session"
    );
    assert_eq!(
        filter_by(&mut app, "demo-app"),
        vec!["bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb".to_string()],
        "the project directory is searchable"
    );
    assert_eq!(
        filter_by(&mut app, "bbbb"),
        vec!["bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb".to_string()],
        "the session id remains searchable"
    );
    assert_eq!(
        filter_by(&mut app, "SEARCH"),
        vec!["bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb".to_string()],
        "matching ignores case"
    );
}
