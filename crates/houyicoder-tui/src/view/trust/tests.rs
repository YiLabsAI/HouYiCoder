//! Trust-screen state and key-routing tests.

use crate::agent_message::{ServerRequest, SessionMessage};
use crate::app::handle_key;
use crate::composition;
use crate::pending_prompt::PendingPrompt;
use crate::state::{Screen, TrustChoice};
use crate::test_harness::{
    TransportEvent, attach_connection, connected_app_with_events, render_text,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::trust::TrustPrompt;
use std::time::Duration;

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
    assert_eq!(
        app.sessions.current.prompt.as_ref().map(|p| p.req_id()),
        Some(RequestId(3))
    );
}

#[test]
fn test_trust_down_selects_exit() {
    let mut app = composition::app();
    attach_connection(&mut app);
    app.sessions.current.prompt = Some(PendingPrompt::trust_ask(
        RequestId(4),
        trust_prompt("/proj"),
    ));
    handle_key(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.trust_choice(), TrustChoice::Exit);
    handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.pending_trust().is_none());
    assert!(app.ui.quit);
}

#[test]
fn test_login_enter_accepts_trust() {
    let mut app = composition::app();
    attach_connection(&mut app);
    app.ui.navigation.screen = Screen::Login;
    app.sessions.current.prompt = Some(PendingPrompt::trust_ask(
        RequestId(5),
        trust_prompt("/proj"),
    ));
    handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.pending_trust().is_none());
    assert!(app.sessions.current.prompt.is_none());
}

#[test]
fn test_resolve_trust_accept_clears() {
    let mut app = composition::app();
    attach_connection(&mut app);
    app.sessions.current.prompt = Some(PendingPrompt::trust_ask(
        RequestId(7),
        trust_prompt("/proj"),
    ));
    app.resolve_trust(true);
    assert!(app.pending_trust().is_none(), "accept clears the ask");
    assert!(
        app.sessions.current.prompt.is_none(),
        "accept clears the req_id"
    );
}

/// resolve_trust(false) (decline) also clears the card — the server shuts
/// the session down, but the TUI state must not leave a stale card up.
#[test]
fn test_resolve_trust_decline_clears() {
    let mut app = composition::app();
    attach_connection(&mut app);
    app.sessions.current.prompt = Some(PendingPrompt::trust_ask(
        RequestId(9),
        trust_prompt("/proj"),
    ));
    app.resolve_trust(false);
    assert!(app.pending_trust().is_none(), "decline clears the ask");
    assert!(
        app.sessions.current.prompt.is_none(),
        "decline clears the req_id"
    );
    assert!(app.ui.quit, "decline exits the TUI");
}

/// resolve_trust with no ask pending is a no-op (the user pressed the key
/// with no card up): nothing panics, nothing mutates.
#[test]
fn test_resolve_trust_noop_empty() {
    let mut app = composition::app();
    app.resolve_trust(true);
    assert!(app.pending_trust().is_none());
    assert!(app.sessions.current.prompt.is_none());
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
}

/// Up on the trust screen reselects Accept.
#[test]
fn test_trust_up_selects_accept() {
    let mut app = composition::app();
    app.sessions.current.prompt = Some(PendingPrompt::trust_ask(
        RequestId(2),
        trust_prompt("/proj"),
    ));
    app.set_trust_choice(TrustChoice::Exit);
    handle_key(&mut app, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.trust_choice(), TrustChoice::Accept);
}

/// A connection loss while the startup trust card is up invalidates the card
/// locally rather than leaving it to trap every key against a transport that
/// can never deliver a verdict. No fake decline verdict is shipped: the host
/// hears nothing, so the gate is the local frontend's to clear. The project
/// path and cause reach the transcript so the user sees why the answer never
/// landed.
#[test]
fn test_trust_loss_invalidates() {
    let (mut app, events) = connected_app_with_events();
    app.sessions.current.prompt = Some(PendingPrompt::trust_ask(
        RequestId(11),
        trust_prompt("/secret/proj"),
    ));
    app.apply_connection_loss("driver exited".into(), Vec::new());

    assert!(app.pending_trust().is_none(), "loss clears the trust card");
    assert!(
        app.sessions.current.prompt.is_none(),
        "loss clears the prompt"
    );
    // No fake verdict: the host never receives a trust reply it never asked
    // the user for. Drain whatever the driver already shipped and confirm none
    // carries the verdict.
    let mut shipped_verdict = false;
    while let Ok(TransportEvent::Frame(frame)) = events.recv_timeout(Duration::from_millis(50)) {
        if frame.contains("TrustVerdict") {
            shipped_verdict = true;
        }
    }
    assert!(!shipped_verdict, "loss ships no fake trust verdict");
    let rendered = render_text(&app, 100, 30);
    assert!(
        rendered.contains("/secret/proj"),
        "transcript surfaces the project path"
    );
    assert!(
        rendered.contains("driver exited"),
        "transcript surfaces the cause"
    );
}

/// After a loss invalidates the trust card, the exit sequence is no longer
/// swallowed: the first Ctrl+D raises the exit confirmation and the second
/// quits, because handle_key routes past the dead card to the base surface.
#[test]
fn test_loss_unblocks_exit() {
    let (mut app, _events) = connected_app_with_events();
    app.sessions.current.prompt = Some(PendingPrompt::trust_ask(
        RequestId(12),
        trust_prompt("/proj"),
    ));
    // While the card is up, Ctrl+D is swallowed by the trust handler.
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
    );
    assert!(
        !app.ui.quit,
        "Ctrl+D does not exit while the trust card is up"
    );
    // The loss invalidates the card.
    app.apply_connection_loss("driver exited".into(), Vec::new());
    assert!(app.pending_trust().is_none());
    // Now the exit sequence reaches the base surface.
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
    );
    assert!(
        app.ui
            .notifications
            .current()
            .is_some_and(|n| n.key == "exit-confirm"),
        "first Ctrl+D raises exit confirmation after loss"
    );
    assert!(
        render_text(&app, 100, 30).contains("Press Ctrl+D again to exit"),
        "exit confirmation is visible"
    );
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
    );
    assert!(
        app.ui.quit,
        "second Ctrl+D exits after loss cleared the trust card"
    );
}

/// A pending trust ask renders the project path + the two-button prompt,
/// so the user sees which folder is asking + how to answer.
#[test]
fn test_renders_trust_prompt() {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    app.sessions.current.prompt = Some(PendingPrompt::trust_ask(
        RequestId(1),
        TrustPrompt {
            project_path: "/home/alice/proj".into(),
            risks: Vec::new(),
        },
    ));
    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("Trust this workspace?"),
        "title missing:\n{out}"
    );
    assert!(
        out.contains("/home/alice/proj"),
        "project path missing:\n{out}"
    );
    assert!(
        out.contains("Yes, I trust this folder"),
        "accept hint missing:\n{out}"
    );
    assert!(out.contains("No, exit"), "decline hint missing:\n{out}");
    assert!(out.contains("› Yes, I trust this folder"));
    app.set_trust_choice(TrustChoice::Exit);
    let out = render_text(&app, 80, 24);
    assert!(out.contains("› No, exit"));
}

#[test]
fn test_long_path_stays_single() {
    let mut app = composition::app();
    app.sessions.current.prompt = Some(PendingPrompt::trust_ask(
        RequestId(1),
        TrustPrompt {
            project_path: "/a/very/long/workspace/path/that/cannot/fit/target-repository".into(),
            risks: Vec::new(),
        },
    ));
    let out = render_text(&app, 40, 18);
    assert!(out.contains('…'), "long path is visibly truncated: {out}");
    assert!(
        out.contains("target-repository"),
        "path tail survives: {out}"
    );
    assert_eq!(
        out.lines()
            .filter(|line| line.contains("target-repository"))
            .count(),
        1,
        "path stays on one row: {out}"
    );
}

/// No pending trust ask: draw is a no-op (the common case), so the
/// trust banner must not appear.
#[test]
fn test_no_render_without_ask() {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    let out = render_text(&app, 80, 24);
    assert!(
        !out.contains("Trust this workspace?"),
        "trust banner must not render with no ask pending:\n{out}"
    );
}

/// While a trust ask is pending, the main view is not drawn — trust is
/// the sole setup screen, not a popup over a live chat. The working
/// surface input placeholder is absent; only the banner renders.
#[test]
fn test_trust_is_sole_screen() {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    app.sessions.current.prompt = Some(PendingPrompt::trust_ask(
        RequestId(1),
        TrustPrompt {
            project_path: "/home/alice/proj".into(),
            risks: Vec::new(),
        },
    ));
    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("Trust this workspace?"),
        "banner present:\n{out}"
    );
    assert!(
        !out.contains("let's build"),
        "main view must not draw while trust pending:\n{out}"
    );
}
