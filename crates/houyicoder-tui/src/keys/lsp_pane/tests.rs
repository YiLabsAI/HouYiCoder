//! /lsp pane key behavior: the two levels' navigation grammar, the detail
//! scroll, and the row gating that refuses an action key its selected row
//! does not support before anything is sent.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use houyicoder_protocol::envelope::{ClientFrame, RequestId};
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::lsp::{LspConfigScope, LspServerState, LspServerStatusView};

use crate::composition;
use crate::keys::handle_working;
use crate::state::{App, Pane, Screen};
use crate::test_harness::{TransportEvent, live_session, render_text, wait_for_request};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn char_key(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

fn row(name: &str, state: LspServerState, locked: bool) -> LspServerStatusView {
    let stderr: Vec<String> = (0..40).map(|i| format!("diagnostic line {i}")).collect();
    LspServerStatusView {
        name: name.into(),
        scope: LspConfigScope::User,
        state,
        locked,
        command_line: "rust-analyzer".into(),
        extensions: vec!["rs".into()],
        open_documents: 1,
        diagnostic_count: 0,
        recent_stderr: stderr,
    }
}

fn pane_app(rows: Vec<LspServerStatusView>) -> App {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    app.ui.navigation.pane = Pane::Lsp;
    app.ui.lsp_pane.mark_pending(RequestId(1));
    app.ui.lsp_pane.apply_status(RequestId(1), rows);
    app
}

fn press(app: &mut App, k: KeyEvent) {
    handle_working(app, k);
}

/// The list grammar: Down and Up move the cursor, Enter opens the detail,
/// Esc returns to the list, and a second Esc closes the pane.
#[test]
fn test_list_keys_navigate() {
    let mut app = pane_app(vec![
        row("alpha", LspServerState::Running, false),
        row("beta", LspServerState::Running, false),
    ]);
    press(&mut app, key(KeyCode::Down));
    assert_eq!(app.ui.lsp_pane.cursor, 1, "Down moves the cursor");
    press(&mut app, key(KeyCode::Up));
    assert_eq!(app.ui.lsp_pane.cursor, 0, "Up moves it back");
    press(&mut app, key(KeyCode::Enter));
    assert_eq!(app.ui.lsp_pane.level, 1, "Enter opens the detail");
    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("Up/Down to scroll"),
        "the detail's hints show: {out}"
    );
    press(&mut app, key(KeyCode::Esc));
    assert_eq!(app.ui.lsp_pane.level, 0, "Esc returns to the list");
    press(&mut app, key(KeyCode::Esc));
    assert_eq!(
        app.ui.navigation.pane,
        Pane::Transcript,
        "a second Esc closes the pane"
    );
    assert!(app.ui.lsp_pane.rows.is_empty(), "closing resets the state");
}

/// In the detail, Up and Down scroll the body rather than moving the list
/// cursor, so the row the detail shows never changes under the keys.
#[test]
fn test_detail_keys_scroll_body() {
    let mut app = pane_app(vec![row("alpha", LspServerState::Running, false)]);
    app.ui.lsp_pane.open_detail();
    let before = render_text(&app, 80, 24);
    assert!(
        before.contains("command  rust-analyzer"),
        "the body starts at its top: {before}"
    );
    press(&mut app, key(KeyCode::PageDown));
    let after = render_text(&app, 80, 24);
    assert!(
        !after.contains("command  rust-analyzer"),
        "the body scrolled past its top: {after}"
    );
    assert_eq!(app.ui.lsp_pane.cursor, 0, "the list cursor never moved");
    press(&mut app, key(KeyCode::PageUp));
    let restored = render_text(&app, 80, 24);
    assert!(
        restored.contains("command  rust-analyzer"),
        "paging back returns to the top: {restored}"
    );
}

/// A locked row refuses both action keys: nothing is sent and nothing is
/// marked pending, so the pane stays exactly as it was.
#[test]
fn test_locked_row_blocks_actions() {
    let (mut app, events) = live_session();
    app.ui.navigation.pane = Pane::Lsp;
    app.ui.lsp_pane.mark_pending(RequestId(1));
    app.ui.lsp_pane.apply_status(
        RequestId(1),
        vec![row(
            "gated",
            LspServerState::Disabled {
                reason: "the workspace is not trusted".into(),
            },
            true,
        )],
    );
    press(&mut app, char_key('r'));
    press(&mut app, char_key('t'));
    assert!(
        app.ui.lsp_pane.pending.is_none(),
        "a refused key sends nothing"
    );
    let mut saw_action = false;
    while let Ok(TransportEvent::Frame(frame)) = events.try_recv() {
        if let Ok(ClientFrame::Request(env)) = serde_json::from_str::<ClientFrame>(&frame)
            && matches!(
                env.payload,
                FrontendRequest::LspRestart { .. } | FrontendRequest::LspSetEnabled { .. }
            )
        {
            saw_action = true;
        }
    }
    assert!(!saw_action, "no action frame left the transport");
}

/// A user-disabled row refuses the restart but still toggles, and a live
/// row sends both: the pane only offers what its row can act on.
#[test]
fn test_row_actions_send_queries() {
    let (mut app, events) = live_session();
    app.ui.navigation.pane = Pane::Lsp;
    app.ui.lsp_pane.mark_pending(RequestId(1));
    app.ui.lsp_pane.apply_status(
        RequestId(1),
        vec![
            row(
                "off",
                LspServerState::Disabled {
                    reason: "disabled by the user".into(),
                },
                false,
            ),
            row("live", LspServerState::Running, false),
        ],
    );
    // The user-disabled row: no live connection to restart.
    press(&mut app, char_key('r'));
    assert!(
        app.ui.lsp_pane.pending.is_none(),
        "a disabled row has nothing to restart"
    );
    // The same row's toggle is an enable request.
    press(&mut app, char_key('t'));
    assert!(app.ui.lsp_pane.pending.is_some(), "the toggle is pending");
    wait_for_request(&events, |payload| {
        matches!(
            payload,
            FrontendRequest::LspSetEnabled { name, enabled } if name == "off" && *enabled
        )
    });
    // The running row: restart asks for a fresh launch.
    press(&mut app, key(KeyCode::Down));
    press(&mut app, char_key('r'));
    wait_for_request(
        &events,
        |payload| matches!(payload, FrontendRequest::LspRestart { name } if name == "live"),
    );
}

/// A project-scope toggle still sends the request, and its receipt says the
/// choice lasts this session because that scope does not persist it.
#[test]
fn test_project_toggle_receipt() {
    let (mut app, events) = live_session();
    app.ui.navigation.pane = Pane::Lsp;
    app.ui.lsp_pane.mark_pending(RequestId(1));
    let mut project = row("shared", LspServerState::Running, false);
    project.scope = LspConfigScope::Project;
    app.ui.lsp_pane.apply_status(RequestId(1), vec![project]);
    press(&mut app, char_key('t'));
    wait_for_request(&events, |payload| {
        matches!(
            payload,
            FrontendRequest::LspSetEnabled { name, enabled } if name == "shared" && !*enabled
        )
    });
    let receipt = app
        .ui
        .lsp_pane
        .pending
        .as_ref()
        .and_then(|pending| pending.receipt.as_ref())
        .expect("the toggle records its receipt");
    assert_eq!(receipt.line(), "lsp: shared disabled (this session only)");
}

/// While the pane is open its surface retracts the input box, so a letter
/// key neither types nor acts: the pane consumes what it owns and drops the
/// rest, the same contract as the sibling panes.
#[test]
fn test_unbound_letter_ignored() {
    let mut app = pane_app(vec![row("alpha", LspServerState::Running, false)]);
    press(&mut app, char_key('x'));
    assert!(
        app.ui.lsp_pane.pending.is_none(),
        "an unbound letter sends nothing"
    );
    assert!(
        app.ui.input.editor.is_empty(),
        "the pane retracts the input box"
    );
}
