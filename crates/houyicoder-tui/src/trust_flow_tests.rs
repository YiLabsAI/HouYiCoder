//! Trust-gate flow tests split from run_control_tests.rs for file-size.
//! The trust card is simpler than the approval flow (no runner, no tool
//! resume): resolve_trust just clears the pending ask + ships the reverse
//! verdict. The ship is a no-op without a wired session, so these tests
//! pin the state mutation (the card clears on a verdict).

use crate::composition;
use houyicoder_protocol::envelope::RequestId;

/// resolve_trust(true) clears the pending trust ask + its req_id, so the
/// card disappears and the server is told to proceed. The reverse verdict
/// is shipped via send_cmd (no-op without a wired session here); the state
/// clear is what the test pins.
#[test]
fn test_resolve_trust_accept_clears() {
    let mut app = composition::app();
    app.pending_trust = Some(houyicoder_protocol::frontend::trust::TrustPrompt {
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
    app.pending_trust = Some(houyicoder_protocol::frontend::trust::TrustPrompt {
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
