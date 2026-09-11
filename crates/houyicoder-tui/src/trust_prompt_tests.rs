//! Trust-screen state and key-routing tests.

use crate::agent_message::AgentMessage;
use crate::composition;
use crate::state::{Screen, TrustChoice};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::trust::TrustPrompt;

/// resolve_trust(true) clears the pending trust ask + its req_id, so the
/// card disappears and the server is told to proceed. The reverse verdict
/// is shipped via send_cmd (no-op without a wired session here); the state
/// clear is what the test pins.
#[test]
fn test_trust_ask_resets_choice() {
    let mut app = composition::app();
    app.trust_choice = TrustChoice::Exit;
    app.handle_agent_message(AgentMessage::TrustAsk {
        req_id: RequestId(3),
        prompt: TrustPrompt {
            project_path: "/proj".into(),
            risks: Vec::new(),
        },
    });
    assert_eq!(app.trust_choice, TrustChoice::Accept);
    assert!(app.pending_trust.is_some());
    assert_eq!(app.pending_trust_req_id, Some(RequestId(3)));
}

#[test]
fn test_trust_down_selects_exit() {
    let mut app = composition::app();
    app.pending_trust = Some(TrustPrompt {
        project_path: "/proj".into(),
        risks: Vec::new(),
    });
    app.pending_trust_req_id = Some(RequestId(4));
    crate::app::handle_key(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.trust_choice, TrustChoice::Exit);
    crate::app::handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.pending_trust.is_none());
    assert!(app.quit);
}

#[test]
fn test_login_enter_accepts_trust() {
    let mut app = composition::app();
    app.screen = Screen::Login;
    app.pending_trust = Some(TrustPrompt {
        project_path: "/proj".into(),
        risks: Vec::new(),
    });
    app.pending_trust_req_id = Some(RequestId(5));
    crate::app::handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.pending_trust.is_none());
    assert!(app.pending_trust_req_id.is_none());
}

#[test]
fn test_resolve_trust_accept_clears() {
    let mut app = composition::app();
    app.pending_trust = Some(TrustPrompt {
        project_path: "/proj".into(),
        risks: Vec::new(),
    });
    app.pending_trust_req_id = Some(RequestId(7));
    app.resolve_trust(true);
    assert!(app.pending_trust.is_none(), "accept clears the ask");
    assert!(
        app.pending_trust_req_id.is_none(),
        "accept clears the req_id"
    );
}

/// resolve_trust(false) (decline) also clears the card — the server shuts
/// the session down, but the TUI state must not leave a stale card up.
#[test]
fn test_resolve_trust_decline_clears() {
    let mut app = composition::app();
    app.pending_trust = Some(TrustPrompt {
        project_path: "/proj".into(),
        risks: Vec::new(),
    });
    app.pending_trust_req_id = Some(RequestId(9));
    app.resolve_trust(false);
    assert!(app.pending_trust.is_none(), "decline clears the ask");
    assert!(
        app.pending_trust_req_id.is_none(),
        "decline clears the req_id"
    );
    assert!(app.quit, "decline exits the TUI");
}

/// resolve_trust with no ask pending is a no-op (the user pressed the key
/// with no card up): nothing panics, nothing mutates.
#[test]
fn test_resolve_trust_noop_empty() {
    let mut app = composition::app();
    app.resolve_trust(true);
    assert!(app.pending_trust.is_none());
    assert!(app.pending_trust_req_id.is_none());
    app.resolve_trust(false);
    assert!(app.pending_trust.is_none());
}
