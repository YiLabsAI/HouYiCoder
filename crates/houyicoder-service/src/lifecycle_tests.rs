//! Tests for the lifecycle store, the registry round trip, and the
//! state-machine terminal-state guards. Extracted from the lifecycle module
//! so the module source stays under the file-size gate; the #[path] include
//! in lifecycle.rs pulls this in for the test build only.

use super::*;
use std::path::PathBuf;

fn record(sid: SessionId) -> SessionRecord {
    SessionRecord {
        session_id: sid,
        event_cursor: houyicoder_context::EventId::new(),
        pending: None,
        runner_checkpoint: Vec::new(),
        lease_holder: None,
    }
}

fn record_with_pending(sid: SessionId) -> SessionRecord {
    SessionRecord {
        session_id: sid,
        event_cursor: houyicoder_context::EventId::new(),
        pending: Some(PendingTurn {
            remaining: vec![PendingPermission {
                call_id: "call_1".into(),
                tool: "bash".into(),
                input: serde_json::json!({"cmd": "ls"}),
            }],
            decided: Vec::new(),
        }),
        runner_checkpoint: Vec::new(),
        lease_holder: None,
    }
}

fn fresh_registry_dir() -> PathBuf {
    // A per-test scratch dir under the system temp. Named by a ULID so
    // parallel tests do not collide; the test drops the dir on its way
    // out so the temp stays clean.
    let mut p = std::env::temp_dir();
    p.push(format!("lc-test-{}", houyicoder_context::SessionId::new()));
    std::fs::create_dir_all(&p).expect("scratch dir");
    p
}

#[test]
fn test_load_session_takes_lease() {
    let store = SessionLeaseStore::new();
    let sid = SessionId::new();
    store.insert(record(sid));
    let r = futures::executor::block_on(store.load_session(sid)).expect("load");
    assert!(r.lease_holder.is_some(), "lease auto-takes on reconnect");
}

#[test]
fn test_cancel_clears_pending_state() {
    let store = SessionLeaseStore::new();
    let sid = SessionId::new();
    store.insert(record(sid));
    futures::executor::block_on(store.cancel(sid)).expect("cancel");
    assert_eq!(store.state(sid), LifecycleState::Cancelled);
}

#[test]
fn test_registry_round_trips_record() {
    let dir = fresh_registry_dir();
    let reg = FileSessionRegistry::open(dir.clone()).expect("open");
    let sid = SessionId::new();
    let rec = record_with_pending(sid);
    reg.register(&rec).expect("register");
    let loaded = reg.load(sid).expect("load").expect("found");
    assert_eq!(loaded.session_id, sid);
    assert!(loaded.pending.is_some(), "pending survives the round trip");
    let pending = loaded.pending.unwrap();
    assert_eq!(
        pending.remaining.len(),
        1,
        "the parked turn retains its one unanswered ask",
    );
    assert_eq!(pending.remaining[0].call_id, "call_1");
    assert!(pending.decided.is_empty(), "no verdicts decided yet");
    reg.remove(sid).expect("remove");
    assert!(reg.load(sid).expect("load after remove").is_none());
    drop(std::fs::remove_dir_all(&dir));
}

#[test]
fn test_load_session_hydrates_registry() {
    let dir = fresh_registry_dir();
    let reg = std::sync::Arc::new(FileSessionRegistry::open(dir.clone()).expect("open"));
    let sid = SessionId::new();
    // Process A: register + detach (no pending, so the persisted state
    // is Detached with lease_holder None).
    let store_a = SessionLeaseStore::with_registry(reg.clone());
    store_a.insert(record(sid));
    futures::executor::block_on(store_a.detach(sid)).expect("detach");
    // Process B: a fresh store with the same registry, no in-memory
    // record. load_session must hydrate from the registry.
    let store_b = SessionLeaseStore::with_registry(reg.clone());
    assert_eq!(
        store_b.state(sid),
        LifecycleState::Startup,
        "pre-load state"
    );
    let r = futures::executor::block_on(store_b.load_session(sid)).expect("hydrate");
    assert_eq!(r.session_id, sid);
    assert!(r.lease_holder.is_some(), "reattach takes the lease");
    assert_eq!(store_b.state(sid), LifecycleState::Running);
    drop(std::fs::remove_dir_all(&dir));
}

#[test]
fn test_rehydrate_pending_permission_load() {
    let dir = fresh_registry_dir();
    let reg = std::sync::Arc::new(FileSessionRegistry::open(dir.clone()).expect("open"));
    let sid = SessionId::new();
    let store_a = SessionLeaseStore::with_registry(reg.clone());
    store_a.insert(record_with_pending(sid));
    futures::executor::block_on(store_a.detach(sid)).expect("detach");
    // The persisted record retains the pending ask across the detach.
    let store_b = SessionLeaseStore::with_registry(reg.clone());
    let r = futures::executor::block_on(store_b.load_session(sid)).expect("hydrate");
    assert!(r.pending.is_some(), "pending ask is retained");
    assert_eq!(
        store_b.state(sid),
        LifecycleState::PendingPermission,
        "reattach with pending lands in PendingPermission"
    );
    drop(std::fs::remove_dir_all(&dir));
}

#[test]
fn test_take_control_rejected_session() {
    let store = SessionLeaseStore::new();
    let sid = SessionId::new();
    store.insert(record(sid));
    futures::executor::block_on(store.cancel(sid)).expect("cancel");
    let err = futures::executor::block_on(store.take_control(sid, false))
        .expect_err("take on cancelled must fail");
    assert!(matches!(err, LifecycleError::InvalidTransition(_)));
}

#[test]
fn test_detach_rejected_cancelled_session() {
    let store = SessionLeaseStore::new();
    let sid = SessionId::new();
    store.insert(record(sid));
    futures::executor::block_on(store.cancel(sid)).expect("cancel");
    let err =
        futures::executor::block_on(store.detach(sid)).expect_err("detach on cancelled must fail");
    assert!(matches!(err, LifecycleError::InvalidTransition(_)));
}

#[test]
fn test_handoff_rejected_cancelled_session() {
    let store = SessionLeaseStore::new();
    let sid = SessionId::new();
    store.insert(record(sid));
    futures::executor::block_on(store.cancel(sid)).expect("cancel");
    let err =
        futures::executor::block_on(store.handoff(sid, houyicoder_context::AgentId("t".into())))
            .expect_err("handoff on cancelled must fail");
    assert!(matches!(err, LifecycleError::InvalidTransition(_)));
}

#[test]
fn test_handoff_handoff_is_noop() {
    let store = SessionLeaseStore::new();
    let sid = SessionId::new();
    store.insert(record(sid));
    futures::executor::block_on(store.handoff(sid, houyicoder_context::AgentId("t".into())))
        .expect("handoff");
    assert_eq!(store.state(sid), LifecycleState::Shutdown);
    // A second handoff on the already-Shutdown session does not error.
    futures::executor::block_on(store.handoff(sid, houyicoder_context::AgentId("t".into())))
        .expect("second handoff is no-op");
    assert_eq!(store.state(sid), LifecycleState::Shutdown);
}

#[test]
fn test_cancel_rejected_shutdown_session() {
    let store = SessionLeaseStore::new();
    let sid = SessionId::new();
    store.insert(record(sid));
    futures::executor::block_on(store.handoff(sid, houyicoder_context::AgentId("t".into())))
        .expect("handoff");
    let err =
        futures::executor::block_on(store.cancel(sid)).expect_err("cancel on shutdown must fail");
    assert!(matches!(err, LifecycleError::InvalidTransition(_)));
}

#[test]
fn test_cancel_cancelled_is_idempotent() {
    let store = SessionLeaseStore::new();
    let sid = SessionId::new();
    store.insert(record(sid));
    futures::executor::block_on(store.cancel(sid)).expect("first cancel");
    // A double-abort from a buggy client does not surface an error.
    futures::executor::block_on(store.cancel(sid)).expect("second cancel is no-op");
    assert_eq!(store.state(sid), LifecycleState::Cancelled);
}

#[test]
fn test_session_cancelled_no_revive() {
    let store = SessionLeaseStore::new();
    let sid = SessionId::new();
    store.insert(record(sid));
    futures::executor::block_on(store.cancel(sid)).expect("cancel");
    // A terminal session returns its record for inspection but does not
    // revive: no lease auto-take, state stays Cancelled.
    let r = futures::executor::block_on(store.load_session(sid)).expect("load");
    assert!(
        r.lease_holder.is_none(),
        "terminal load does not take the lease"
    );
    assert!(r.pending.is_none(), "cancel reaped the pending ask");
    assert_eq!(store.state(sid), LifecycleState::Cancelled);
}

#[test]
fn test_take_control_reaps_pending() {
    let store = SessionLeaseStore::new();
    let sid = SessionId::new();
    store.insert(record_with_pending(sid));
    // A held lease + force: the take succeeds and reaps the pending ask.
    futures::executor::block_on(store.take_control(sid, true)).expect("force take");
    assert_eq!(store.state(sid), LifecycleState::Running);
    let r = futures::executor::block_on(store.load_session(sid)).expect("load");
    assert!(r.pending.is_none(), "force take reaps pending");
}

#[test]
fn test_persist_detach_survives_process() {
    let dir = fresh_registry_dir();
    let reg = std::sync::Arc::new(FileSessionRegistry::open(dir.clone()).expect("open"));
    let sid = SessionId::new();
    let store_a = SessionLeaseStore::with_registry(reg.clone());
    store_a.insert(record(sid));
    futures::executor::block_on(store_a.take_control(sid, false)).expect("take");
    // The persisted record reflects the new lease holder.
    let loaded = reg.load(sid).expect("load").expect("found");
    assert!(loaded.lease_holder.is_some(), "persist reflects the take");
    drop(std::fs::remove_dir_all(&dir));
}

#[test]
fn test_handoff_removes_persisted_record() {
    let dir = fresh_registry_dir();
    let reg = std::sync::Arc::new(FileSessionRegistry::open(dir.clone()).expect("open"));
    let sid = SessionId::new();
    let store = SessionLeaseStore::with_registry(reg.clone());
    store.insert(record(sid));
    futures::executor::block_on(store.handoff(sid, houyicoder_context::AgentId("t".into())))
        .expect("handoff");
    // Handoff is terminal and drops the persisted file so a reconnect
    // sees NotFound, matching in-memory state.
    assert!(
        reg.load(sid).expect("load").is_none(),
        "handoff drops the file"
    );
    drop(std::fs::remove_dir_all(&dir));
}
