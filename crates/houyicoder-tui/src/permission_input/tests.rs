use houyicoder_protocol::frontend::FrontendRequest;

use super::*;
use crate::test_harness::{connected_app_events, wait_for_request};

/// Removing a cached working directory ships a PermissionRemoveWorkingDir
/// request naming its path for the server to drop.
#[test]
fn test_remove_dir_ships() {
    let (mut app, events) = connected_app_events();
    app.sessions.current.capabilities.permission.dirs_cache = vec!["/tmp/extra".into()];
    remove_dir_at(&mut app, 0);
    let req = wait_for_request(&events, |p| {
        matches!(p, FrontendRequest::PermissionRemoveWorkingDir { .. })
    });
    assert_eq!(req.req_id.0, 0, "first request on a fresh session");
    match req.payload {
        FrontendRequest::PermissionRemoveWorkingDir { path } => assert_eq!(path, "/tmp/extra"),
        other => panic!("unexpected request: {other:?}"),
    }
}
