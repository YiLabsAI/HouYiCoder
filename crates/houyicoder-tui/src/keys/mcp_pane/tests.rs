//! /mcp pane key behavior: the two levels' navigation grammar, the detail
//! scroll, and the row gating that refuses an action key its selected row
//! does not support before anything is sent.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use houyicoder_protocol::envelope::{ClientFrame, RequestId};
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::mcp::{
    McpConfigScope, McpConnectionState, McpServerDetailView, McpServerStatusView,
};

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

fn row(name: &str, state: McpConnectionState, locked: bool) -> McpServerStatusView {
    let stderr: Vec<String> = (0..40).map(|i| format!("diagnostic line {i}")).collect();
    McpServerStatusView {
        name: name.into(),
        scope: McpConfigScope::User,
        state,
        server_version: None,
        tool_count: 1,
        resource_count: 0,
        has_instructions: false,
        recent_stderr: stderr,
        detail: McpServerDetailView {
            endpoint: "fake-server".into(),
            header_names: Vec::new(),
            tools: Vec::new(),
            instructions_preview: None,
            untrusted_lock: locked,
        },
    }
}

fn pane_app(rows: Vec<McpServerStatusView>) -> App {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    app.ui.navigation.pane = Pane::Mcp;
    app.ui.mcp_pane.mark_pending(RequestId(1));
    app.ui.mcp_pane.apply_status(RequestId(1), rows);
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
        row("alpha", McpConnectionState::Connected, false),
        row("beta", McpConnectionState::Connected, false),
    ]);
    press(&mut app, key(KeyCode::Down));
    assert_eq!(app.ui.mcp_pane.cursor, 1, "Down moves the cursor");
    press(&mut app, key(KeyCode::Up));
    assert_eq!(app.ui.mcp_pane.cursor, 0, "Up moves it back");
    press(&mut app, key(KeyCode::Enter));
    assert_eq!(app.ui.mcp_pane.level, 1, "Enter opens the detail");
    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("Up/Down to scroll"),
        "the detail's hints show: {out}"
    );
    press(&mut app, key(KeyCode::Esc));
    assert_eq!(app.ui.mcp_pane.level, 0, "Esc returns to the list");
    press(&mut app, key(KeyCode::Esc));
    assert_eq!(
        app.ui.navigation.pane,
        Pane::Transcript,
        "a second Esc closes the pane"
    );
    assert!(app.ui.mcp_pane.rows.is_empty(), "closing resets the state");
}

/// In the detail, Up and Down scroll the body rather than moving the list
/// cursor, so the row the detail shows never changes under the keys.
#[test]
fn test_detail_keys_scroll_body() {
    let mut app = pane_app(vec![row("alpha", McpConnectionState::Connected, false)]);
    app.ui.mcp_pane.open_detail();
    let before = render_text(&app, 80, 24);
    assert!(
        before.contains("transport"),
        "the body starts at its top: {before}"
    );
    press(&mut app, key(KeyCode::PageDown));
    let after = render_text(&app, 80, 24);
    assert!(
        !after.contains("transport  fake-server"),
        "the body scrolled past its top: {after}"
    );
    assert_eq!(app.ui.mcp_pane.cursor, 0, "the list cursor never moved");
    press(&mut app, key(KeyCode::PageUp));
    let restored = render_text(&app, 80, 24);
    assert!(
        restored.contains("transport  fake-server"),
        "paging back returns to the top: {restored}"
    );
}

/// A locked row refuses both action keys: nothing is sent and nothing is
/// marked pending, so the pane stays exactly as it was.
#[test]
fn test_locked_row_blocks_actions() {
    let (mut app, events) = live_session();
    app.ui.navigation.pane = Pane::Mcp;
    app.ui.mcp_pane.mark_pending(RequestId(1));
    app.ui.mcp_pane.apply_status(
        RequestId(1),
        vec![row(
            "gated",
            McpConnectionState::Disabled {
                reason: "the workspace is not trusted".into(),
            },
            true,
        )],
    );
    press(&mut app, char_key('r'));
    press(&mut app, char_key('t'));
    assert!(
        app.ui.mcp_pane.pending.is_none(),
        "a refused key sends nothing"
    );
    let mut saw_action = false;
    while let Ok(TransportEvent::Frame(frame)) = events.try_recv() {
        if let Ok(ClientFrame::Request(env)) = serde_json::from_str::<ClientFrame>(&frame)
            && matches!(
                env.payload,
                FrontendRequest::McpReconnect { .. } | FrontendRequest::McpSetEnabled { .. }
            )
        {
            saw_action = true;
        }
    }
    assert!(!saw_action, "no action frame left the transport");
}

/// A user-disabled row refuses the reconnect but still toggles, and a live
/// row sends both: the pane only offers what its row can act on.
#[test]
fn test_row_actions_send_queries() {
    let (mut app, events) = live_session();
    app.ui.navigation.pane = Pane::Mcp;
    app.ui.mcp_pane.mark_pending(RequestId(1));
    app.ui.mcp_pane.apply_status(
        RequestId(1),
        vec![
            row(
                "off",
                McpConnectionState::Disabled {
                    reason: "disabled by the user".into(),
                },
                false,
            ),
            row("live", McpConnectionState::Connected, false),
        ],
    );
    // The user-disabled row: no reconnect to ask for.
    press(&mut app, char_key('r'));
    assert!(
        app.ui.mcp_pane.pending.is_none(),
        "a disabled row has nothing to reconnect"
    );
    // The same row's toggle is an enable request.
    press(&mut app, char_key('t'));
    assert!(app.ui.mcp_pane.pending.is_some(), "the toggle is pending");
    wait_for_request(&events, |payload| {
        matches!(
            payload,
            FrontendRequest::McpSetEnabled { name, enabled } if name == "off" && *enabled
        )
    });
    // The connected row: reconnect asks for a fresh handshake.
    press(&mut app, key(KeyCode::Down));
    press(&mut app, char_key('r'));
    wait_for_request(
        &events,
        |payload| matches!(payload, FrontendRequest::McpReconnect { name } if name == "live"),
    );
}

/// While the pane is open its surface retracts the input box, so a letter
/// key neither types nor acts: the pane consumes what it owns and drops the
/// rest, the same contract as the sibling panes.
#[test]
fn test_unbound_letter_ignored() {
    let mut app = pane_app(vec![row("alpha", McpConnectionState::Connected, false)]);
    press(&mut app, char_key('x'));
    assert!(
        app.ui.mcp_pane.pending.is_none(),
        "an unbound letter sends nothing"
    );
    assert!(
        app.ui.input.editor.is_empty(),
        "the pane retracts the input box"
    );
}
