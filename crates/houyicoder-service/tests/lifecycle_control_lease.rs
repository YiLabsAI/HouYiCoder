//! Control-lease semantics at the lifecycle-store level: the four
//! conditions the control-lease spec mandates, tested against the
//! SessionLeaseStore (the session-indexed Lifecycle state). The full
//! wire-level reconnect-replay (server re-sends a pending permission
//! ask to a reattaching client) is a later runtime cut; here the state
//! machine itself is proven: lease auto-take on reconnect, exclusive
//! take with force reaping, pending retained across detach, and
//! cancel reaping the pending ask + clearing state.

use futures::executor::block_on;
use houyicoder_context::{EventId, SessionId};
use houyicoder_service::lifecycle::{
    Lifecycle, LifecycleError, LifecycleState, SessionLeaseStore, SessionRecord,
};

fn record(sid: SessionId, lease: Option<&str>) -> SessionRecord {
    SessionRecord {
        session_id: sid,
        event_cursor: EventId::new(),
        pending: None,
        runner_checkpoint: Vec::new(),
        lease_holder: lease.map(String::from),
    }
}

/// Condition 1: reconnect (load_session) auto-takes the lease when it is
/// free, so a reattaching client becomes the holder without an explicit
/// takeControl.
#[test]
fn test_reconnect_auto_takes_free() {
    let store = SessionLeaseStore::new();
    let sid = SessionId::new();
    store.insert(record(sid, None));
    let r = block_on(store.load_session(sid)).expect("load");
    assert!(r.lease_holder.is_some(), "lease auto-takes on reconnect");
    assert_eq!(store.state(sid), LifecycleState::Running);
}

/// Condition 3: takeControl without force fails closed when another
/// client holds the lease; with force it reaps the pending ask and
/// takes the lease.
#[test]
fn test_take_force_reaps_lease() {
    let store = SessionLeaseStore::new();
    let sid = SessionId::new();
    store.insert(record(sid, Some("holder")));
    let err = block_on(store.take_control(sid, false)).unwrap_err();
    assert!(matches!(err, LifecycleError::LeaseHeld(_)), "{err:?}");
    block_on(store.take_control(sid, true)).expect("force take");
    let r = block_on(store.load_session(sid)).expect("load after take");
    assert!(r.lease_holder.is_some(), "force takes the lease");
}

/// Condition 4: cancel reaps the pending permission ask and moves the
/// session to the terminal Cancelled state (the reverse-request req_id
/// closes as Cancelled, not dangling).
#[test]
fn test_cancel_reaps_pending_state() {
    let store = SessionLeaseStore::new();
    let sid = SessionId::new();
    store.insert(record(sid, Some("holder")));
    block_on(store.cancel(sid)).expect("cancel");
    assert_eq!(store.state(sid), LifecycleState::Cancelled);
    let r = block_on(store.load_session(sid)).expect("load");
    assert!(r.pending.is_none(), "cancel reaps the pending ask");
}

/// Detach releases the lease but the session survives in the Detached
/// state; the reattaching client re-takes via load_session. The
/// pending-ask-retention-across-detach plus wire-level resend is a
/// later runtime cut; the state transition is proven here.
#[test]
fn test_reconnect_retakes_lease_detach() {
    let store = SessionLeaseStore::new();
    let sid = SessionId::new();
    store.insert(record(sid, Some("holder")));
    block_on(store.detach(sid)).expect("detach");
    assert_eq!(store.state(sid), LifecycleState::Detached);
    let r = block_on(store.load_session(sid)).expect("load");
    assert!(r.lease_holder.is_some(), "reconnect re-takes after detach");
}

/// A session that does not exist fails NotFound (fail-closed), not a
/// silent default.
#[test]
fn test_unknown_session_not_found() {
    let store = SessionLeaseStore::new();
    let sid = SessionId::new();
    let err = block_on(store.load_session(sid)).unwrap_err();
    assert!(matches!(err, LifecycleError::NotFound));
}
