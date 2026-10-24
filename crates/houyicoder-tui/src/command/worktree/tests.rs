use crate::composition::{WorktreeEntry, app};
use crate::records::TranscriptLine;

fn row(path: &str) -> WorktreeEntry {
    WorktreeEntry {
        path: path.into(),
        head: "abcdef0".into(),
        branch: "main".into(),
        is_current: false,
        ..Default::default()
    }
}

/// The cursor clamps to the list: moving past the last row holds at the
/// last, moving above the first holds at the first. No out-of-bounds
/// index for the Enter / d actions to dereference.
#[test]
fn test_worktree_cursor_clamps() {
    let mut app = app();
    app.sessions.current.capabilities.worktree.entries = vec![row("/a"), row("/b"), row("/c")];
    app.move_worktree_cursor(1);
    assert_eq!(app.ui.worktree_pane.list.cursor, 1, "down one");
    app.move_worktree_cursor(100);
    assert_eq!(
        app.ui.worktree_pane.list.cursor, 2,
        "past the end clamps to the last row"
    );
    app.move_worktree_cursor(-100);
    assert_eq!(
        app.ui.worktree_pane.list.cursor, 0,
        "past the start clamps to the first row"
    );
}

/// An empty list is a no-op (no panic, cursor stays at zero) so opening
/// the pane in a non-repo directory does not index into nothing.
#[test]
fn test_worktree_cursor_noop_empty() {
    let mut app = app();
    app.sessions.current.capabilities.worktree.entries = Vec::new();
    app.move_worktree_cursor(1);
    assert_eq!(
        app.ui.worktree_pane.list.cursor, 0,
        "empty list keeps cursor at zero"
    );
}

/// refresh_worktrees with no working directory clears the list + resets
/// the cursor, so the pane renders the fallback message rather than stale
/// rows from a previous repo.
#[test]
fn test_refresh_clears_without_dir() {
    let mut app = app();
    app.sessions.current.capabilities.worktree.entries = vec![row("/stale")];
    app.ui.worktree_pane.list.cursor = 0;
    app.sessions.current.identity.working_dir = String::new();
    app.refresh_worktrees();
    assert!(
        app.sessions
            .current
            .capabilities
            .worktree
            .entries
            .is_empty(),
        "list cleared"
    );
}

#[test]
fn test_enter_noop_empty() {
    let mut app = app();
    app.sessions.current.capabilities.worktree.entries = Vec::new();
    app.enter_worktree_at_cursor();
}

#[test]
fn test_cursor_search_skips_filtered() {
    let mut app = app();
    app.sessions.current.capabilities.worktree.entries =
        vec![row("/alpha"), row("/beta"), row("/gamma")];
    app.ui.worktree_pane.list.query = "beta".into();
    app.move_worktree_cursor(1);
    // Filtered has 1 item; cursor jumps to it.
    assert_eq!(
        app.ui.worktree_pane.list.cursor, 1,
        "search cursor lands on the filtered item"
    );
}

#[test]
fn test_cursor_search_empty_noop() {
    let mut app = app();
    app.sessions.current.capabilities.worktree.entries = vec![row("/a"), row("/b")];
    app.ui.worktree_pane.list.query = "zzz".into();
    app.move_worktree_cursor(1);
    // No filtered items: cursor stays put.
    assert_eq!(
        app.ui.worktree_pane.list.cursor, 0,
        "empty search results keep cursor"
    );
}

#[test]
fn test_enter_no_carrier() {
    let mut app = app();
    app.sessions.current.capabilities.worktree.entries = vec![row("/some/wt")];
    app.ui.worktree_pane.list.cursor = 0;
    // Disconnected: the enter path reports it and enters nothing.
    app.enter_worktree_at_cursor();
    assert!(
        app.sessions
            .current
            .transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::System(s) if s.contains("not connected"))),
        "disconnected enter should report not connected"
    );
}
