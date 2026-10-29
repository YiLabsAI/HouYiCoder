//! Key-routing tests for the /worktrees pane.

use std::sync::mpsc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::FrontendRequest;

use crate::agent_message::{ServerResponse, SessionMessage};
use crate::composition;
use crate::keys::handle_working;
use crate::state::{App, Pane};
use crate::test_harness::{
    TransportEvent, connected_app_with_events, wait_for_request, worktree_preview,
};
use crate::ui::{RemoveFocus, WorktreeLevel};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// An App on the /worktrees pane with three rows, its own connection so the
/// pane's requests reach a recording transport, and the cursor on the first.
fn worktree_app() -> (App, mpsc::Receiver<TransportEvent>) {
    let (mut app, events) = connected_app_with_events();
    app.ui.navigation.pane = Pane::Worktree;
    app.sessions.current.capabilities.worktree.entries = vec![
        entry("/w/alpha", "main"),
        entry("/w/beta", "dev"),
        entry("/w/gamma", "feat"),
    ];
    app.ui.input.editor.clear();
    (app, events)
}

fn entry(path: &str, branch: &str) -> composition::WorktreeEntry {
    composition::WorktreeEntry {
        path: path.into(),
        head: "abcdef0".into(),
        branch: branch.into(),
        is_current: false,
        ..Default::default()
    }
}

/// Answer the preview the pane asked for, the way the host would.
fn answer_preview(app: &mut App, req: RequestId, slug: &str) {
    app.handle_agent_message(SessionMessage::Response {
        request: req,
        response: ServerResponse::WorktreePreview {
            preview: worktree_preview(slug),
        },
    });
}

#[test]
fn test_worktree_cursor_down_up() {
    let (mut app, _events) = worktree_app();
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

/// Enter opens the detail for the row under the cursor and asks the host what
/// a removal would discard. No run starts, so nothing reaches the model.
#[test]
fn test_worktree_enter_opens_detail() {
    let (mut app, events) = worktree_app();
    handle_working(&mut app, key(KeyCode::Down));
    handle_working(&mut app, key(KeyCode::Enter));
    assert_eq!(
        app.ui.worktree_pane.level.get(),
        WorktreeLevel::Detail,
        "Enter opens the detail"
    );
    let env = wait_for_request(
        &events,
        |payload| matches!(payload, FrontendRequest::WorktreePreview { path } if path == "/w/beta"),
    );
    answer_preview(&mut app, env.req_id, "beta");
    assert_eq!(
        app.ui
            .worktree_pane
            .draft
            .borrow()
            .preview()
            .map(|p| p.name.as_str()),
        Some("beta"),
        "the host's preview fills the detail"
    );
    assert!(
        app.active_run_req_id().is_none(),
        "opening a detail starts no run"
    );
}

/// Removing takes two deliberate moves: the default button cancels, and only
/// the Remove button submits the preview's token.
#[test]
fn test_worktree_remove_needs_focus() {
    let (mut app, events) = worktree_app();
    handle_working(&mut app, key(KeyCode::Char('d')));
    assert_eq!(
        app.ui.worktree_pane.level.get(),
        WorktreeLevel::List,
        "the confirmation waits for the host's answer"
    );
    let env = wait_for_request(
        &events,
        |payload| matches!(payload, FrontendRequest::WorktreePreview { path } if path == "/w/alpha"),
    );
    answer_preview(&mut app, env.req_id, "alpha");
    assert_eq!(
        app.ui.worktree_pane.level.get(),
        WorktreeLevel::Confirm,
        "the answer opens the confirmation"
    );
    assert_eq!(
        app.ui.worktree_pane.draft.borrow().focus(),
        RemoveFocus::Cancel,
        "the row starts on Cancel"
    );
    // Enter on Cancel backs out one level and sends nothing.
    handle_working(&mut app, key(KeyCode::Enter));
    assert_eq!(
        app.ui.worktree_pane.level.get(),
        WorktreeLevel::List,
        "Cancel returns to the list"
    );
    // Reopening reuses the held preview, so no second read is asked for.
    handle_working(&mut app, key(KeyCode::Char('d')));
    handle_working(&mut app, key(KeyCode::Right));
    assert_eq!(
        app.ui.worktree_pane.draft.borrow().focus(),
        RemoveFocus::Remove,
        "Right moves to Remove"
    );
    handle_working(&mut app, key(KeyCode::Enter));
    let removal = wait_for_request(&events, |payload| {
        matches!(payload, FrontendRequest::WorktreeRemove { .. })
    });
    // Only the Remove press took an id: Cancel consumed none.
    assert_eq!(
        removal.req_id,
        RequestId(1),
        "the confirmation sent one request, not two"
    );
    match removal.payload {
        FrontendRequest::WorktreeRemove {
            path,
            token,
            delete_unmerged_branch,
        } => {
            assert_eq!(path, "/w/alpha");
            assert_eq!(token, "digest-1");
            assert!(
                !delete_unmerged_branch,
                "an unmerged branch is kept by default"
            );
        }
        other => panic!("expected a removal request, got {other:?}"),
    }
    assert!(
        app.ui.worktree_pane.draft.borrow().removing(),
        "the confirmation waits on its own removal"
    );
}

/// The repository's own working tree is named in the list rather than taken
/// to a confirmation whose only possible answer is a refusal.
#[test]
fn test_main_row_refuses_removal() {
    let (mut app, _events) = worktree_app();
    app.sessions.current.capabilities.worktree.entries[0].is_main = true;
    handle_working(&mut app, key(KeyCode::Char('d')));
    assert_eq!(
        app.ui.worktree_pane.level.get(),
        WorktreeLevel::List,
        "no page opens for a tree that cannot be removed"
    );
    assert!(
        app.ui
            .worktree_pane
            .notice()
            .is_some_and(|notice| notice.contains("main working tree")),
        "the list says why"
    );
    assert!(
        app.ui.worktree_pane.draft.borrow().path().is_none(),
        "no removal is prepared"
    );
    assert!(
        app.ui
            .worktree_pane
            .draft
            .borrow()
            .preview_request()
            .is_none(),
        "the host is not asked to read a tree that cannot go"
    );
}

/// The branch choice is a separate toggle, and it rides the same removal.
#[test]
fn test_branch_key_marks_deletion() {
    let (mut app, events) = worktree_app();
    handle_working(&mut app, key(KeyCode::Char('d')));
    let env = wait_for_request(&events, |payload| {
        matches!(payload, FrontendRequest::WorktreePreview { .. })
    });
    answer_preview(&mut app, env.req_id, "alpha");
    handle_working(&mut app, key(KeyCode::Char('b')));
    assert!(
        app.ui.worktree_pane.draft.borrow().delete_unmerged_branch(),
        "b marks the branch for deletion"
    );
    handle_working(&mut app, key(KeyCode::Right));
    handle_working(&mut app, key(KeyCode::Enter));
    let removal = wait_for_request(&events, |payload| {
        matches!(payload, FrontendRequest::WorktreeRemove { .. })
    });
    assert!(
        matches!(
            removal.payload,
            FrontendRequest::WorktreeRemove {
                delete_unmerged_branch: true,
                ..
            }
        ),
        "the toggle rides the removal"
    );
}

/// The list's e action asks the host to move the session into the worktree.
#[test]
fn test_worktree_e_enters_tree() {
    let (mut app, events) = worktree_app();
    handle_working(&mut app, key(KeyCode::Char('e')));
    wait_for_request(
        &events,
        |payload| matches!(payload, FrontendRequest::WorktreeEnter { path } if path == "/w/alpha"),
    );
    assert!(
        app.active_run_req_id().is_none(),
        "entering a worktree starts no run"
    );
}

/// / starts the search so bare letters stay actions, and Esc backs out of the
/// search before it closes the pane.
#[test]
fn test_worktree_slash_starts_search() {
    let (mut app, _events) = worktree_app();
    handle_working(&mut app, key(KeyCode::Char('e')));
    assert!(
        !app.ui.worktree_pane.list.searching()
            && app.ui.worktree_pane.draft.borrow().path().is_none(),
        "a bare letter is not typed into the query"
    );
    handle_working(&mut app, key(KeyCode::Char('/')));
    handle_working(&mut app, key(KeyCode::Char('b')));
    handle_working(&mut app, key(KeyCode::Char('e')));
    handle_working(&mut app, key(KeyCode::Backspace));
    assert_eq!(
        app.ui.worktree_pane.list.query, "b",
        "the search box takes characters"
    );
    handle_working(&mut app, key(KeyCode::Enter));
    assert!(
        app.ui.worktree_pane.list.searching(),
        "Enter keeps the query as the filter"
    );
    assert!(
        !app.ui.worktree_pane.search.get(),
        "Enter leaves the search box"
    );
    handle_working(&mut app, key(KeyCode::Esc));
    assert!(
        !app.ui.worktree_pane.list.searching(),
        "Esc clears the filter"
    );
    assert_eq!(
        app.ui.navigation.pane,
        Pane::Worktree,
        "Esc with a filter stays in the pane"
    );
    handle_working(&mut app, key(KeyCode::Esc));
    assert_eq!(
        app.ui.navigation.pane,
        Pane::Transcript,
        "Esc at the root closes the pane"
    );
}

/// Esc from the detail returns to the list and keeps the held preview.
#[test]
fn test_worktree_esc_returns() {
    let (mut app, _events) = worktree_app();
    handle_working(&mut app, key(KeyCode::Enter));
    handle_working(&mut app, key(KeyCode::Esc));
    assert_eq!(app.ui.worktree_pane.level.get(), WorktreeLevel::List);
    assert_eq!(
        app.ui.navigation.pane,
        Pane::Worktree,
        "Esc from the detail keeps the pane open"
    );
}

/// A preview that answers a request the pane no longer waits on is dropped,
/// so it cannot overwrite a newer read.
#[test]
fn test_worktree_late_preview_dropped() {
    let (mut app, _events) = worktree_app();
    handle_working(&mut app, key(KeyCode::Enter));
    answer_preview(&mut app, RequestId(99), "alpha");
    assert!(
        app.ui.worktree_pane.draft.borrow().preview().is_none(),
        "an answer to another request fills nothing"
    );
    answer_preview(&mut app, RequestId(0), "alpha");
    assert!(
        app.ui.worktree_pane.draft.borrow().preview().is_some(),
        "the answer the pane waits on fills the detail"
    );
}

#[test]
fn test_worktree_unknown_key_noop() {
    let (mut app, _events) = worktree_app();
    let before = app.ui.worktree_pane.list.cursor;
    handle_working(&mut app, key(KeyCode::F(1)));
    assert_eq!(
        app.ui.worktree_pane.list.cursor, before,
        "unknown key does not move cursor"
    );
    assert!(
        !app.ui.worktree_pane.search.get(),
        "unknown key does not open search"
    );
}

/// The detail keeps the list's two actions: e moves the session in, d opens
/// the confirmation from where the user already is, once the read it was
/// already waiting on lands.
#[test]
fn test_detail_keys_reach_actions() {
    let (mut app, events) = worktree_app();
    handle_working(&mut app, key(KeyCode::Enter));
    let env = wait_for_request(&events, |payload| {
        matches!(payload, FrontendRequest::WorktreePreview { .. })
    });
    handle_working(&mut app, key(KeyCode::Char('e')));
    wait_for_request(
        &events,
        |payload| matches!(payload, FrontendRequest::WorktreeEnter { path } if path == "/w/alpha"),
    );
    handle_working(&mut app, key(KeyCode::Char('d')));
    answer_preview(&mut app, env.req_id, "alpha");
    assert_eq!(
        app.ui.worktree_pane.level.get(),
        WorktreeLevel::Confirm,
        "d opens the confirmation from the detail"
    );
}

/// r reads the tree again without leaving the confirmation, so a stale answer
/// is followed by a fresh one rather than by a new decision.
#[test]
fn test_confirm_rereads_tree() {
    let (mut app, events) = worktree_app();
    handle_working(&mut app, key(KeyCode::Char('d')));
    let first = wait_for_request(&events, |payload| {
        matches!(payload, FrontendRequest::WorktreePreview { .. })
    });
    answer_preview(&mut app, first.req_id, "alpha");
    handle_working(&mut app, key(KeyCode::Char('r')));
    let second = wait_for_request(&events, |payload| {
        matches!(payload, FrontendRequest::WorktreePreview { .. })
    });
    assert_ne!(second.req_id, first.req_id, "the reread is a new request");
    assert_eq!(
        app.ui.worktree_pane.level.get(),
        WorktreeLevel::Confirm,
        "the confirmation stays open"
    );
}

/// Esc while the search box is open empties it and leaves the pane, so the
/// next key acts on the list again.
#[test]
fn test_search_esc_clears_filter() {
    let (mut app, _events) = worktree_app();
    handle_working(&mut app, key(KeyCode::Char('/')));
    handle_working(&mut app, key(KeyCode::Char('b')));
    handle_working(&mut app, key(KeyCode::Enter));
    handle_working(&mut app, key(KeyCode::Esc));
    assert_eq!(app.ui.worktree_pane.list.query, "", "the filter is dropped");
    assert_eq!(
        app.ui.navigation.pane,
        Pane::Worktree,
        "the pane stays open"
    );
}

/// Letters stay the message while the input box holds text, so e and d never
/// act on a worktree instead of the draft the user is writing.
#[test]
fn test_typing_keeps_letters() {
    let (mut app, _events) = worktree_app();
    app.ui.input.editor.set("describe the tree".to_string());
    handle_working(&mut app, key(KeyCode::Char('e')));
    handle_working(&mut app, key(KeyCode::Char('d')));
    assert_eq!(app.ui.worktree_pane.level.get(), WorktreeLevel::List);
    assert!(
        app.ui.worktree_pane.draft.borrow().path().is_none(),
        "no worktree action taken while typing"
    );
}

/// Control-modified letters belong to the editor, so Ctrl+E does not move
/// the session and Ctrl+D opens no removal.
#[test]
fn test_editor_keeps_ctrl_keys() {
    let (mut app, _events) = worktree_app();
    handle_working(
        &mut app,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
    );
    assert!(
        app.ui
            .worktree_pane
            .draft
            .borrow()
            .enter_request()
            .is_none(),
        "Ctrl+E asks the host for nothing"
    );
    handle_working(
        &mut app,
        KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
    );
    assert_eq!(
        app.ui.worktree_pane.level.get(),
        WorktreeLevel::List,
        "Ctrl+D opens no confirmation"
    );
}

/// Up and Down move the block at the detail and the confirmation, where the
/// rows are what the keys act on, and never move the list's cursor from there.
#[test]
fn test_scroll_keys_move_block() {
    let (mut app, events) = worktree_app();
    handle_working(&mut app, key(KeyCode::Enter));
    let env = wait_for_request(&events, |payload| {
        matches!(payload, FrontendRequest::WorktreePreview { .. })
    });
    let cursor = app.ui.worktree_pane.list.cursor;
    handle_working(&mut app, key(KeyCode::Down));
    assert_eq!(
        app.ui.worktree_pane.scroll.get(),
        1,
        "Down moves the block down a row"
    );
    handle_working(&mut app, key(KeyCode::Down));
    assert_eq!(app.ui.worktree_pane.scroll.get(), 2, "Down again");
    handle_working(&mut app, key(KeyCode::Up));
    assert_eq!(app.ui.worktree_pane.scroll.get(), 1, "Up moves back");
    assert_eq!(
        app.ui.worktree_pane.list.cursor, cursor,
        "the list's cursor is not what these keys move"
    );
    // The confirmation scrolls the same way, since the preview it shows is
    // just as long as the detail's.
    answer_preview(&mut app, env.req_id, "alpha");
    handle_working(&mut app, key(KeyCode::Char('d')));
    handle_working(&mut app, key(KeyCode::Down));
    assert_eq!(
        app.ui.worktree_pane.scroll.get(),
        1,
        "the confirmation opens at the top, so one press moves it a row"
    );
    handle_working(&mut app, key(KeyCode::Up));
    handle_working(&mut app, key(KeyCode::Up));
    assert_eq!(
        app.ui.worktree_pane.scroll.get(),
        0,
        "scroll stops at the first row"
    );
}

/// Up and Down move through the filtered rows while the search box is open,
/// so the list is navigable before the query is committed.
#[test]
fn test_search_keeps_cursor_keys() {
    let (mut app, _events) = worktree_app();
    handle_working(&mut app, key(KeyCode::Char('/')));
    handle_working(&mut app, key(KeyCode::Char('a')));
    handle_working(&mut app, key(KeyCode::Down));
    assert_eq!(app.ui.worktree_pane.list.cursor, 1, "Down moves the cursor");
    handle_working(&mut app, key(KeyCode::Down));
    assert_eq!(app.ui.worktree_pane.list.cursor, 2, "Down again");
    handle_working(&mut app, key(KeyCode::Up));
    assert_eq!(app.ui.worktree_pane.list.cursor, 1, "Up moves back");
    assert_eq!(app.ui.worktree_pane.list.query, "a", "the query is kept");
}
