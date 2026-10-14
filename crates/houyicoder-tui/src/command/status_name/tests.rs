use super::*;
use crate::composition::app;
use crate::test_harness::{connected_app_events, wait_for_request};
use houyicoder_protocol::frontend::FrontendRequest;

/// enter opens the editor even without an active session; the session
/// check lives at commit (a disconnected app can still open the editor;
/// Enter reports not connected there).
#[test]
fn test_enter_opens_without_session() {
    let mut app = app();
    app.sessions.current.connection = None;
    app.enter_status_name_edit();
    assert!(
        app.ui.navigation.status_name_edit.is_some(),
        "editor opens without a session (commit is the gate)"
    );
}

/// cancel drops the editor without shipping a request.
#[test]
fn test_cancel_drops_editor() {
    let mut app = app();
    app.ui.navigation.status_name_edit = Some(InputField::new());
    app.cancel_status_name_edit();
    assert!(app.ui.navigation.status_name_edit.is_none());
}

/// commit with no editor is a no-op (does not panic).
#[test]
fn test_commit_no_editor_noop() {
    let mut app = app();
    app.commit_status_name_edit();
    assert!(app.ui.navigation.status_name_edit.is_none());
}

/// Commit with a typed name ships a RenameSession request under the
/// current session id.
#[test]
fn test_commit_ships_rename() {
    let (mut app, events) = connected_app_events();
    let mut field = InputField::new();
    field.set("my session".into());
    app.ui.navigation.status_name_edit = Some(field);
    app.commit_status_name_edit();
    let req = wait_for_request(&events, |p| {
        matches!(p, FrontendRequest::RenameSession { .. })
    });
    assert_eq!(req.req_id.0, 0, "first request on a fresh session");
    match req.payload {
        FrontendRequest::RenameSession { name, .. } => assert_eq!(name, "my session"),
        other => panic!("unexpected request: {other:?}"),
    }
}
