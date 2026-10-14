//! Tests for this module.

use super::*;
use houyicoder_protocol::acp_wire::{AcpRequest, AcpRequestId, JsonRpcVersion};

fn req(method: &str, id: i64) -> AcpRequest {
    AcpRequest {
        jsonrpc: JsonRpcVersion::V2,
        id: AcpRequestId::Number(id),
        method: method.into(),
        params: None,
    }
}

#[test]
fn test_initialize_replies_acpx_caps() {
    let caps = AcpxCapabilities {
        streaming: true,
        cas: false,
        detach: true,
        ext_methods: vec!["acpx/session/takeControl".into()],
    };
    let adapter = AcpAdapter::new(caps, 1, SessionLeaseStore::new());
    let resp = futures::executor::block_on(adapter.handle(&req("initialize", 5)));
    match resp {
        AcpResponse::Result { result, .. } => {
            let s = result.to_string();
            assert!(s.contains(r#""_meta":{"acpx""#), "{s}");
            assert!(s.contains(r#""loadSession":true"#), "{s}");
            assert!(
                s.contains(r#""extMethods":["acpx/session/takeControl"]"#),
                "{s}"
            );
        }
        AcpResponse::Error { .. } => panic!("initialize must succeed"),
    }
}

#[test]
fn test_unknown_method_not_found() {
    let adapter = AcpAdapter::new(AcpxCapabilities::default(), 1, SessionLeaseStore::new());
    let resp = futures::executor::block_on(adapter.handle(&req("bogus", 9)));
    match resp {
        AcpResponse::Error { error, id, .. } => {
            assert!(matches!(id, AcpRequestId::Number(9)));
            assert_eq!(error.code, AcpErrorCode::MethodNotFound);
        }
        AcpResponse::Result { .. } => panic!("unknown method must error"),
    }
}

#[test]
fn test_new_session_mints_stores() {
    let adapter = AcpAdapter::new(AcpxCapabilities::default(), 1, SessionLeaseStore::new());
    let req = AcpRequest {
        jsonrpc: JsonRpcVersion::V2,
        id: AcpRequestId::Number(2),
        method: "session/new".into(),
        params: Some(serde_json::json!({"cwd": "/tmp"})),
    };
    let resp = futures::executor::block_on(adapter.handle(&req));
    match resp {
        AcpResponse::Result { result, .. } => {
            assert!(
                result.get("sessionId").is_some(),
                "missing sessionId: {result}"
            );
        }
        AcpResponse::Error { .. } => panic!("new_session must succeed"),
    }
}

#[test]
fn test_load_session_reattaches_known() {
    let store = SessionLeaseStore::new();
    let sid = SessionId::new();
    store.insert(SessionRecord {
        session_id: sid,
        event_cursor: EventId::new(),
        pending: None,
        runner_checkpoint: Vec::new(),
        lease_holder: None,
    });
    let adapter = AcpAdapter::new(AcpxCapabilities::default(), 1, store);
    let req = AcpRequest {
        jsonrpc: JsonRpcVersion::V2,
        id: AcpRequestId::Number(3),
        method: "session/load".into(),
        params: Some(serde_json::json!({"sessionId": sid.to_string(), "cwd": "/tmp"})),
    };
    let resp = futures::executor::block_on(adapter.handle(&req));
    assert!(
        matches!(resp, AcpResponse::Result { .. }),
        "load must succeed"
    );
}

#[test]
fn test_load_session_unknown_fails() {
    let adapter = AcpAdapter::new(AcpxCapabilities::default(), 1, SessionLeaseStore::new());
    // A real ULID-shaped id that no session is registered under.
    let sid = SessionId::new().to_string();
    let req = AcpRequest {
        jsonrpc: JsonRpcVersion::V2,
        id: AcpRequestId::Number(4),
        method: "session/load".into(),
        params: Some(serde_json::json!({"sessionId": sid, "cwd": "/tmp"})),
    };
    let resp = futures::executor::block_on(adapter.handle(&req));
    match resp {
        AcpResponse::Error { error, .. } => {
            assert_eq!(error.code, AcpErrorCode::ResourceNotFound);
        }
        AcpResponse::Result { .. } => panic!("unknown session must error"),
    }
}

#[test]
fn test_take_control_grants_lease() {
    let store = SessionLeaseStore::new();
    let sid = SessionId::new();
    store.insert(SessionRecord {
        session_id: sid,
        event_cursor: EventId::new(),
        pending: None,
        runner_checkpoint: Vec::new(),
        lease_holder: None,
    });
    let adapter = AcpAdapter::new(AcpxCapabilities::default(), 1, store);
    let req = AcpRequest {
        jsonrpc: JsonRpcVersion::V2,
        id: AcpRequestId::Number(7),
        method: "_acpx/session/takeControl".into(),
        params: Some(serde_json::json!({"sessionId": sid.to_string()})),
    };
    let resp = futures::executor::block_on(adapter.handle(&req));
    match resp {
        AcpResponse::Result { result, .. } => {
            assert!(
                result.to_string().contains(r#""type":"granted""#),
                "{result}"
            );
        }
        AcpResponse::Error { .. } => panic!("takeControl must succeed"),
    }
}

/// A parked PendingTurn is detected on takeControl: the adapter reports
/// pending_resent: true so the new lease holder knows a pending permission
/// ask exists to re-emit. This exercises the detection mechanism at the
/// adapter boundary; the actual re-send over a persistent transport is the
/// activation step (lands when a UDS+ACP serve path drives this adapter).
#[test]
fn test_take_control_reports_pending() {
    let store = SessionLeaseStore::new();
    let sid = SessionId::new();
    store.insert(SessionRecord {
        session_id: sid,
        event_cursor: EventId::new(),
        pending: Some(PendingTurn {
            remaining: vec![PendingPermission {
                call_id: "toolu_1".into(),
                tool: "bash".into(),
                input: serde_json::json!({}),
            }],
            decided: Vec::new(),
        }),
        runner_checkpoint: Vec::new(),
        lease_holder: None,
    });
    let adapter = AcpAdapter::new(AcpxCapabilities::default(), 1, store);
    let req = AcpRequest {
        jsonrpc: JsonRpcVersion::V2,
        id: AcpRequestId::Number(8),
        method: "_acpx/session/takeControl".into(),
        params: Some(serde_json::json!({"sessionId": sid.to_string()})),
    };
    let resp = futures::executor::block_on(adapter.handle(&req));
    match resp {
        AcpResponse::Result { result, .. } => {
            let s = result.to_string();
            assert!(s.contains(r#""type":"granted""#), "{s}");
            assert!(
                s.contains(r#""pending_resent":true"#),
                "must report pending when a PendingTurn is parked: {s}"
            );
        }
        AcpResponse::Error { .. } => panic!("takeControl must succeed"),
    }
}

/// No parked PendingTurn: pending_resent is absent (None), so the new
/// holder is not told to expect a re-emit. The complement of the positive
/// test above, covering the no-pending path.
#[test]
fn test_take_control_no_pending() {
    let store = SessionLeaseStore::new();
    let sid = SessionId::new();
    store.insert(SessionRecord {
        session_id: sid,
        event_cursor: EventId::new(),
        pending: None,
        runner_checkpoint: Vec::new(),
        lease_holder: None,
    });
    let adapter = AcpAdapter::new(AcpxCapabilities::default(), 1, store);
    let req = AcpRequest {
        jsonrpc: JsonRpcVersion::V2,
        id: AcpRequestId::Number(9),
        method: "_acpx/session/takeControl".into(),
        params: Some(serde_json::json!({"sessionId": sid.to_string()})),
    };
    let resp = futures::executor::block_on(adapter.handle(&req));
    match resp {
        AcpResponse::Result { result, .. } => {
            let s = result.to_string();
            assert!(s.contains(r#""type":"granted""#), "{s}");
            assert!(
                !s.contains(r#""pending_resent":true"#),
                "no pending must not report pending_resent true: {s}"
            );
        }
        AcpResponse::Error { .. } => panic!("takeControl must succeed"),
    }
}

#[test]
fn test_take_control_denied_lease() {
    let store = SessionLeaseStore::new();
    let sid = SessionId::new();
    store.insert(SessionRecord {
        session_id: sid,
        event_cursor: EventId::new(),
        pending: None,
        runner_checkpoint: Vec::new(),
        lease_holder: Some("someone".into()),
    });
    let adapter = AcpAdapter::new(AcpxCapabilities::default(), 1, store);
    let req = AcpRequest {
        jsonrpc: JsonRpcVersion::V2,
        id: AcpRequestId::Number(8),
        method: "_acpx/session/takeControl".into(),
        params: Some(serde_json::json!({"sessionId": sid.to_string()})),
    };
    let resp = futures::executor::block_on(adapter.handle(&req));
    match resp {
        AcpResponse::Result { result, .. } => {
            assert!(
                result.to_string().contains(r#""type":"denied""#),
                "{result}"
            );
        }
        AcpResponse::Error { .. } => panic!("held lease must deny, not error"),
    }
}

#[test]
fn test_cancel_notification_reaps_session() {
    let store = SessionLeaseStore::new();
    let sid = SessionId::new();
    store.insert(SessionRecord {
        session_id: sid,
        event_cursor: EventId::new(),
        pending: None,
        runner_checkpoint: Vec::new(),
        lease_holder: Some("x".into()),
    });
    let adapter = AcpAdapter::new(AcpxCapabilities::default(), 1, store.clone());
    let notif = AcpNotification::new(
        "session/cancel",
        serde_json::json!({"sessionId": sid.to_string()}),
    );
    futures::executor::block_on(adapter.handle_notification(&notif));
    assert_eq!(
        store.state(sid),
        crate::lifecycle::LifecycleState::Cancelled
    );
}
