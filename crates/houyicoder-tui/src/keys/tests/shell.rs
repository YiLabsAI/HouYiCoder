//! Esc handles a running direct shell command: it asks the host to stop it
//! and leaves the composer and the queue alone.

use super::*;
use crate::test_harness::{connected_app_events, render_text};

#[test]
fn test_esc_stops_shell() {
    let (mut app, _events) = connected_app_events();
    assert!(app.submit_direct_shell("sleep 30".into()));
    assert!(!app.sessions.current.shell.is_cancelling());
    handle_working(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(
        app.sessions.current.shell.is_cancelling(),
        "Esc asks the host to stop the command"
    );
    let screen = render_text(&app, 100, 24);
    assert!(
        screen.contains("shell stopping"),
        "the status row says the stop is in progress: {screen}"
    );
}
