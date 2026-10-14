use super::*;
use std::sync::mpsc::channel;
#[test]
fn test_palette_nav_no_panic() {
    let mut app = test_app();
    app.open_palette();
    app.palette_up();
    app.palette_down();
    app.palette_push('a');
    app.palette_pop();
}

#[test]
fn test_console_focus_nav() {
    let mut app = test_app();
    app.console_focus_up();
    app.console_focus_down();
}

#[test]
fn test_app_debug_format() {
    let app = test_app();
    drop(format!("{app:?}"));
}

#[test]
fn test_has_connection_reports_presence() {
    let mut app = test_app();
    assert!(!app.has_connection());
    let (_tx, rx) = channel::<crate::agent_message::SessionMessage>();
    app.sessions.current.connection = Some(crate::session::SessionConnection::from_receiver(rx));
    assert!(app.has_connection());
}

#[test]
fn test_stage_label_nonempty() {
    for s in [
        Stage::Idle,
        Stage::Design,
        Stage::Implementing,
        Stage::Verify,
        Stage::Done,
    ] {
        assert!(!s.label().is_empty());
    }
}
