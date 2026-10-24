use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::frontend::permission::{PermissionEffect, RuleDestination};
use ratatui::crossterm::event::KeyCode;

use super::PermissionDecision;
use crate::permission_pane_state::PermissionPaneState;
use crate::state::enums::{PermissionInput, PermissionTab};
use crate::test_harness::{connected_app_events, wait_for_request};

/// Removing a cached working directory ships a PermissionRemoveWorkingDir
/// request naming its path for the server to drop.
#[test]
fn test_remove_dir_ships() {
    let (mut app, events) = connected_app_events();
    app.sessions.current.capabilities.permission.dirs_cache = vec!["/tmp/extra".into()];
    app.remove_dir_at(0);
    let req = wait_for_request(&events, |p| {
        matches!(p, FrontendRequest::PermissionRemoveWorkingDir { .. })
    });
    assert_eq!(req.req_id.0, 0, "first request on a fresh session");
    match req.payload {
        FrontendRequest::PermissionRemoveWorkingDir { path } => assert_eq!(path, "/tmp/extra"),
        other => panic!("unexpected request: {other:?}"),
    }
}

#[test]
fn test_pane_nav_and_modes() {
    let mut pane = PermissionPaneState::default();
    assert!(matches!(
        pane.handle_key(KeyCode::Esc, true, None, 0),
        PermissionDecision::ClosePane
    ));
    assert!(matches!(
        pane.handle_key(KeyCode::Right, true, None, 0),
        PermissionDecision::Consumed {
            clear_editor: false
        }
    ));
    assert_eq!(pane.tab, PermissionTab::Ask);
    assert!(matches!(
        pane.handle_key(KeyCode::Left, true, None, 0),
        PermissionDecision::Consumed {
            clear_editor: false
        }
    ));
    assert_eq!(pane.tab, PermissionTab::Allow);

    // Enter Search, type, navigate, and exit with Enter.
    assert!(matches!(
        pane.handle_key(KeyCode::Char('s'), true, None, 0),
        PermissionDecision::Consumed {
            clear_editor: false
        }
    ));
    assert!(matches!(pane.input, PermissionInput::Search));
    drop(pane.handle_key(KeyCode::Char('g'), true, None, 0));
    drop(pane.handle_key(KeyCode::Backspace, true, None, 0));
    drop(pane.handle_key(KeyCode::Down, true, None, 0));
    drop(pane.handle_key(KeyCode::Up, true, None, 0));
    assert!(matches!(
        pane.handle_key(KeyCode::Enter, true, None, 0),
        PermissionDecision::Consumed { clear_editor: true }
    ));
    assert!(matches!(pane.input, PermissionInput::None));

    // Enter AddDestination, cycle destinations, and commit.
    pane.input = PermissionInput::AddDestination {
        action: "bash".into(),
        content: None,
        effect: PermissionEffect::Allow,
        destination: RuleDestination::Project,
    };
    drop(pane.handle_key(KeyCode::Right, true, None, 0));
    drop(pane.handle_key(KeyCode::Left, true, None, 0));
    drop(pane.handle_key(KeyCode::Tab, true, None, 0));
    assert!(matches!(
        pane.handle_key(KeyCode::Enter, true, None, 0),
        PermissionDecision::AddRule { .. }
    ));

    // Remove rule confirm and cancel paths.
    drop(pane.handle_key(KeyCode::Char('d'), true, Some(2), 0));
    drop(pane.handle_key(KeyCode::Tab, true, None, 0));
    assert!(matches!(
        pane.handle_key(KeyCode::Enter, true, None, 0),
        PermissionDecision::CancelNotice(_)
    ));
    drop(pane.handle_key(KeyCode::Char('d'), true, Some(2), 0));
    drop(pane.handle_key(KeyCode::Right, true, None, 0));
    assert!(matches!(
        pane.handle_key(KeyCode::Enter, true, None, 0),
        PermissionDecision::RemoveRule(2)
    ));

    // Workspace tab directory add/remove sub-modes.
    pane.tab = PermissionTab::Workspace;
    drop(pane.handle_key(KeyCode::Char('a'), true, None, 1));
    assert!(matches!(pane.input, PermissionInput::AddDir));
    drop(pane.handle_key(KeyCode::Esc, true, None, 1));
    drop(pane.handle_key(KeyCode::Char('d'), true, None, 1));
    drop(pane.handle_key(KeyCode::Tab, true, None, 1));
    assert!(matches!(
        pane.handle_key(KeyCode::Enter, true, None, 1),
        PermissionDecision::CancelNotice(_)
    ));
    drop(pane.handle_key(KeyCode::Char('d'), true, None, 1));
    drop(pane.handle_key(KeyCode::Left, true, None, 1));
    assert!(matches!(
        pane.handle_key(KeyCode::Enter, true, None, 1),
        PermissionDecision::RemoveDir(0)
    ));
}
