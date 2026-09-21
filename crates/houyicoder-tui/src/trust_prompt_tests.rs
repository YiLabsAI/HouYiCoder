//! Trust-screen state and key-routing tests.

use crate::agent_message::{ServerRequest, SessionMessage};
use crate::composition;
use crate::pending_prompt::PendingPrompt;
use crate::state::{Screen, TrustChoice};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::trust::TrustPrompt;

/// A trust prompt with the given path.
fn trust_prompt(path: &str) -> TrustPrompt {
    TrustPrompt {
        project_path: path.into(),
        risks: Vec::new(),
    }
}

/// resolve_trust(true) clears the pending trust ask + its req_id, so the
/// card disappears and the server is told to proceed. The reverse verdict
/// is shipped via send (no-op without a wired session here); the state
/// clear is what the test pins.
#[test]
fn test_trust_ask_resets_choice() {
    let mut app = composition::app();
    app.set_trust_choice(TrustChoice::Exit);
    app.handle_agent_message(SessionMessage::Request {
        request: RequestId(3),
        payload: ServerRequest::Trust {
            prompt: trust_prompt("/proj"),
        },
    });
    assert_eq!(app.trust_choice(), TrustChoice::Accept);
    assert!(app.pending_trust().is_some());
    assert_eq!(app.prompt.as_ref().map(|p| p.req_id()), Some(RequestId(3)));
}

#[test]
fn test_trust_down_selects_exit() {
    let mut app = composition::app();
    crate::test_harness::attach_connection(&mut app);
    app.prompt = Some(PendingPrompt::trust_ask(
        RequestId(4),
        trust_prompt("/proj"),
    ));
    crate::app::handle_key(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.trust_choice(), TrustChoice::Exit);
    crate::app::handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.pending_trust().is_none());
    assert!(app.quit);
}

#[test]
fn test_login_enter_accepts_trust() {
    let mut app = composition::app();
    crate::test_harness::attach_connection(&mut app);
    app.screen = Screen::Login;
    app.prompt = Some(PendingPrompt::trust_ask(
        RequestId(5),
        trust_prompt("/proj"),
    ));
    crate::app::handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.pending_trust().is_none());
    assert!(app.prompt.is_none());
}

#[test]
fn test_resolve_trust_accept_clears() {
    let mut app = composition::app();
    crate::test_harness::attach_connection(&mut app);
    app.prompt = Some(PendingPrompt::trust_ask(
        RequestId(7),
        trust_prompt("/proj"),
    ));
    app.resolve_trust(true);
    assert!(app.pending_trust().is_none(), "accept clears the ask");
    assert!(app.prompt.is_none(), "accept clears the req_id");
}

/// resolve_trust(false) (decline) also clears the card — the server shuts
/// the session down, but the TUI state must not leave a stale card up.
#[test]
fn test_resolve_trust_decline_clears() {
    let mut app = composition::app();
    crate::test_harness::attach_connection(&mut app);
    app.prompt = Some(PendingPrompt::trust_ask(
        RequestId(9),
        trust_prompt("/proj"),
    ));
    app.resolve_trust(false);
    assert!(app.pending_trust().is_none(), "decline clears the ask");
    assert!(app.prompt.is_none(), "decline clears the req_id");
    assert!(app.quit, "decline exits the TUI");
}

/// resolve_trust with no ask pending is a no-op (the user pressed the key
/// with no card up): nothing panics, nothing mutates.
#[test]
fn test_resolve_trust_noop_empty() {
    let mut app = composition::app();
    app.resolve_trust(true);
    assert!(app.pending_trust().is_none());
    assert!(app.prompt.is_none());
    app.resolve_trust(false);
    assert!(app.pending_trust().is_none());
}

/// A trust ask is not a permission ask: it exposes no approval card, no
/// interactive question, and zero batched approval requests.
#[test]
fn test_trust_ask_not_permission() {
    let mut p = PendingPrompt::trust_ask(RequestId(1), trust_prompt("/proj"));
    assert_eq!(p.request_count(), 0);
    assert!(p.approval().is_none());
    assert!(p.approval_mut().is_none());
    assert!(p.question().is_none());
    assert!(p.question_mut().is_none());
    assert!(p.take_question().is_none());
}

/// Up on the trust screen reselects Accept.
#[test]
fn test_trust_up_selects_accept() {
    let mut app = composition::app();
    app.prompt = Some(PendingPrompt::trust_ask(
        RequestId(2),
        trust_prompt("/proj"),
    ));
    app.set_trust_choice(TrustChoice::Exit);
    crate::app::handle_key(&mut app, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.trust_choice(), TrustChoice::Accept);
}
