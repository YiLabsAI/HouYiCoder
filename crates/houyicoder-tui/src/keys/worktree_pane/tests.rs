//! Key-routing tests for the /worktrees pane.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::composition;
use crate::keys::handle_working;
use crate::records::TranscriptLine;
use crate::state::{App, Pane};
use crate::test_harness::working_app;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// /worktrees pane key surface: Up/Down move the cursor, Enter/d route to
/// the enter/remove actions (which report not connected while disconnected), Char
/// types into the search query, Backspace edits it, Esc in search clears
/// the query (a second Esc closes), and an unmapped key is a no-op. These
/// cover the worktree pane key handler without a PTY.
fn worktree_app_with_rows() -> App {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Worktree;
    app.sessions.current.capabilities.worktree.entries = vec![
        composition::WorktreeEntry {
            path: "/a".into(),
            head: "abcdef0".into(),
            branch: "main".into(),
            is_current: false,
            ..Default::default()
        },
        composition::WorktreeEntry {
            path: "/b".into(),
            head: "1234567".into(),
            branch: "dev".into(),
            is_current: false,
            ..Default::default()
        },
        composition::WorktreeEntry {
            path: "/c".into(),
            head: "fedcba0".into(),
            branch: "feat".into(),
            is_current: false,
            ..Default::default()
        },
    ];
    app.ui.input.editor.clear();
    app
}

#[test]
fn test_worktree_cursor_down_up() {
    let mut app = worktree_app_with_rows();
    handle_working(&mut app, key(KeyCode::Down));
    assert_eq!(
        app.ui.worktree_pane.list.cursor, 1,
        "Down moves the cursor down"
    );
    handle_working(&mut app, key(KeyCode::Down));
    assert_eq!(app.ui.worktree_pane.list.cursor, 2, "Down again");
    handle_working(&mut app, key(KeyCode::Up));
    assert_eq!(app.ui.worktree_pane.list.cursor, 1, "Up moves back up");
}

#[test]
fn test_worktree_e_enter() {
    let mut app = worktree_app_with_rows();
    // Enter opens the detail (level 1), then 'e' in the detail opens the
    // worktree (the enter ability, kept beyond the display-only round).
    handle_working(&mut app, key(KeyCode::Enter));
    assert_eq!(
        app.ui.worktree_pane.level.get(),
        1,
        "Enter opens the detail"
    );
    handle_working(&mut app, key(KeyCode::Char('e')));
    assert!(
        app.sessions
            .current
            .transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::System(s) if s.contains("not connected"))),
        "e in detail enters the worktree (not connected while disconnected)"
    );
}

#[test]
fn test_worktree_char_into_query() {
    let mut app = worktree_app_with_rows();
    handle_working(&mut app, key(KeyCode::Char('b')));
    handle_working(&mut app, key(KeyCode::Char('e')));
    assert_eq!(
        app.ui.worktree_pane.list.query, "be",
        "char keys append to the query"
    );
    assert!(
        app.ui.worktree_pane.list.searching(),
        "a non-empty query is searching"
    );
}

#[test]
fn test_worktree_backspace_query() {
    let mut app = worktree_app_with_rows();
    handle_working(&mut app, key(KeyCode::Char('a')));
    handle_working(&mut app, key(KeyCode::Char('b')));
    handle_working(&mut app, key(KeyCode::Backspace));
    assert_eq!(
        app.ui.worktree_pane.list.query, "a",
        "Backspace pops the last char"
    );
}

#[test]
fn test_worktree_esc_clears_search() {
    let mut app = worktree_app_with_rows();
    handle_working(&mut app, key(KeyCode::Char('a')));
    assert!(app.ui.worktree_pane.list.searching());
    // First Esc clears the search, not the pane.
    handle_working(&mut app, key(KeyCode::Esc));
    assert!(
        !app.ui.worktree_pane.list.searching(),
        "Esc clears the query"
    );
    assert_eq!(
        app.ui.navigation.pane,
        Pane::Worktree,
        "Esc in search stays in the pane"
    );
    // Second Esc (no search) closes the pane.
    handle_working(&mut app, key(KeyCode::Esc));
    assert_eq!(
        app.ui.navigation.pane,
        Pane::Transcript,
        "Esc with no search closes the pane"
    );
}

#[test]
fn test_worktree_unknown_key_noop() {
    let mut app = worktree_app_with_rows();
    let before = app.ui.worktree_pane.list.cursor;
    // An unmapped function key: the handler does not consume it and leaves
    // the cursor + query untouched.
    handle_working(&mut app, key(KeyCode::F(1)));
    assert_eq!(
        app.ui.worktree_pane.list.cursor, before,
        "unknown key does not move cursor"
    );
    assert!(
        !app.ui.worktree_pane.list.searching(),
        "unknown key does not open search"
    );
}
