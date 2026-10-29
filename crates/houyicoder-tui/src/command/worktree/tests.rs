use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::FrontendRequest;

use crate::agent_message::{ServerResponse, SessionMessage};
use crate::composition::{WorktreeEntry, app};
use crate::test_harness::{connected_app_with_events, wait_for_request, worktree_preview};
use crate::ui::WorktreeLevel;

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

/// A session with no connection cannot enter a worktree or read a removal
/// preview: no request goes out and no run starts, so the pane keeps what it
/// shows rather than waiting on a reply that cannot come.
#[test]
fn test_enter_needs_connection() {
    let mut app = app();
    app.sessions.current.capabilities.worktree.entries = vec![row("/some/wt")];
    app.ui.worktree_pane.list.cursor = 0;
    app.enter_worktree_at_cursor();
    app.open_worktree_detail();
    assert_eq!(
        app.ui.worktree_pane.level.get(),
        WorktreeLevel::Detail,
        "the detail still opens on what the listing holds"
    );
    assert!(
        app.ui.worktree_pane.draft.borrow().preview().is_none(),
        "a preview needs a host to answer it"
    );
    assert!(
        app.active_run_req_id().is_none(),
        "neither action starts a run"
    );
}

/// The confirmation backs out to the level it was opened from, so removing
/// from the detail returns to the detail rather than the list.
#[test]
fn test_close_returns_to_opener() {
    let (mut app, events) = connected_app_with_events();
    app.sessions.current.capabilities.worktree.entries = vec![row("/some/wt")];
    app.ui.worktree_pane.list.cursor = 0;
    app.open_worktree_detail();
    let read = wait_for_request(&events, |payload| {
        matches!(payload, FrontendRequest::WorktreePreview { .. })
    });
    app.handle_agent_message(SessionMessage::Response {
        request: read.req_id,
        response: ServerResponse::WorktreePreview {
            preview: worktree_preview("wt"),
        },
    });
    app.begin_worktree_removal();
    assert_eq!(app.ui.worktree_pane.level.get(), WorktreeLevel::Confirm);
    app.close_worktree_removal();
    assert_eq!(
        app.ui.worktree_pane.level.get(),
        WorktreeLevel::Detail,
        "the confirmation returns to the detail it came from"
    );
}

/// The main working tree is not a worktree the host can measure for removal,
/// so its detail opens on what the listing holds and reads nothing.
#[test]
fn test_main_detail_reads_nothing() {
    let (mut app, _events) = connected_app_with_events();
    let mut main = row("/repo");
    main.is_main = true;
    main.is_current = true;
    app.sessions.current.capabilities.worktree.entries = vec![main];
    app.open_worktree_detail();
    assert_eq!(app.ui.worktree_pane.level.get(), WorktreeLevel::Detail);
    assert!(
        app.ui
            .worktree_pane
            .draft
            .borrow()
            .preview_request()
            .is_none(),
        "no read goes out for the main working tree"
    );
}

/// A removal of the main working tree is refused where the gesture was made,
/// including from the detail, and no confirmation opens on the refusal.
#[test]
fn test_main_removal_refused() {
    let (mut app, _events) = connected_app_with_events();
    let mut main = row("/repo");
    main.is_main = true;
    main.is_current = true;
    app.sessions.current.capabilities.worktree.entries = vec![main];
    app.open_worktree_detail();
    app.begin_worktree_removal();
    assert_eq!(
        app.ui.worktree_pane.level.get(),
        WorktreeLevel::Detail,
        "the confirmation stays closed"
    );
    assert!(
        app.ui
            .worktree_pane
            .draft
            .borrow()
            .preview_request()
            .is_none(),
        "no read goes out"
    );
    assert!(
        app.ui.worktree_pane.draft.borrow().notice().is_some(),
        "the detail says why"
    );
}

/// The tree the session works in cannot be removed either, and the list says
/// so instead of sending a request the host would refuse.
#[test]
fn test_current_removal_refused() {
    let mut app = app();
    let mut current = row("/repo/current");
    current.is_current = true;
    app.sessions.current.capabilities.worktree.entries = vec![current];
    app.begin_worktree_removal();
    assert_eq!(app.ui.worktree_pane.level.get(), WorktreeLevel::List);
    assert!(
        app.ui
            .worktree_pane
            .draft
            .borrow()
            .preview_request()
            .is_none(),
        "no read goes out"
    );
    assert!(app.ui.worktree_pane.notice().is_some(), "the list says why");
}

/// Entering the tree the session already works in changes nothing, so the
/// gesture is answered in the pane rather than sent to the host.
#[test]
fn test_enter_refused_for_current() {
    let (mut app, _events) = connected_app_with_events();
    let mut current = row("/repo/current");
    current.is_current = true;
    app.sessions.current.capabilities.worktree.entries = vec![current];
    app.enter_worktree_at_cursor();
    assert!(
        app.ui
            .worktree_pane
            .draft
            .borrow()
            .enter_request()
            .is_none(),
        "no request goes out"
    );
    assert!(app.ui.worktree_pane.notice().is_some(), "the list says why");
}

/// The read key leaves a removal that is already with the host alone, since a
/// fresh read would blank the page that states where the removal stands.
#[test]
fn test_retry_waits_for_removal() {
    let (mut app, events) = connected_app_with_events();
    app.sessions.current.capabilities.worktree.entries = vec![row("/some/wt")];
    app.ui.worktree_pane.list.cursor = 0;
    app.open_worktree_detail();
    let read = wait_for_request(&events, |payload| {
        matches!(payload, FrontendRequest::WorktreePreview { .. })
    });
    app.handle_agent_message(SessionMessage::Response {
        request: read.req_id,
        response: ServerResponse::WorktreePreview {
            preview: worktree_preview("wt"),
        },
    });
    app.begin_worktree_removal();
    let submitted = app
        .ui
        .worktree_pane
        .draft
        .borrow_mut()
        .take_submission(RequestId(9))
        .is_some();
    assert!(submitted, "the confirmation submits a removal");
    app.retry_worktree_preview();
    assert!(
        app.ui.worktree_pane.draft.borrow().preview().is_some(),
        "the preview the page states the removal against is kept"
    );
    assert!(
        app.ui.worktree_pane.draft.borrow().removing(),
        "the removal stays with the host"
    );
}
