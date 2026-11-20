//! Tests for this module.

use super::*;
use crate::lifecycle::LifecycleState;
use futures::executor::block_on;
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
        ext_methods: vec!["_acpx/session/takeControl".into()],
    };
    let adapter = AcpAdapter::new(caps, 1, SessionLeaseStore::new());
    let resp = block_on(adapter.handle(&req("initialize", 5)));
    match resp {
        AcpResponse::Result { result, .. } => {
            let s = result.to_string();
            assert!(s.contains(r#""_meta":{"acpx""#), "{s}");
            assert!(s.contains(r#""loadSession":true"#), "{s}");
            assert!(
                s.contains(r#""extMethods":["_acpx/session/takeControl"]"#),
                "{s}"
            );
        }
        AcpResponse::Error { .. } => panic!("initialize must succeed"),
    }
}

#[test]
fn test_unknown_method_not_found() {
    let adapter = AcpAdapter::new(AcpxCapabilities::default(), 1, SessionLeaseStore::new());
    let resp = block_on(adapter.handle(&req("bogus", 9)));
    match resp {
        AcpResponse::Error { error, id, .. } => {
            assert!(matches!(id, AcpRequestId::Number(9)));
            assert_eq!(error.code, AcpErrorCode::MethodNotFound);
        }
        AcpResponse::Result { .. } => panic!("unknown method must error"),
    }
}

use std::sync::Arc;
use std::sync::Mutex;

use houyicoder_protocol::acp_wire::McpServer;
use houyicoder_protocol::acp_wire::NewSessionRequest;

/// A bound adapter answers session/new with the runner's own session id and
/// hands the request's workspace facts to the binding, so a stock client
/// that prompts against the returned id drives the session the runner
/// actually runs. The binding records what it saw; the assertions read the
/// recorded request rather than the id alone, so a regression that answers
/// the right id without applying the workspace still fails here.
#[test]
fn test_acp_new_session_binds() {
    let bound_session = SessionId::new();
    let seen: Arc<Mutex<Option<NewSessionRequest>>> = Arc::new(Mutex::new(None));
    let record = Arc::clone(&seen);
    let adapter = AcpAdapter::new(AcpxCapabilities::default(), 1, SessionLeaseStore::new())
        .with_session_binding(Arc::new(move |params: &NewSessionRequest| {
            *record.lock().expect("seen lock") = Some(params.clone());
            bound_session
        }));
    let req = AcpRequest {
        jsonrpc: JsonRpcVersion::V2,
        id: AcpRequestId::Number(9),
        method: "session/new".into(),
        params: Some(serde_json::json!({
            "cwd": "/work/acp",
            "additionalDirectories": ["/work/libs"],
            "mcpServers": [
                {"name": "fs", "command": "/usr/bin/fsmcp", "args": ["--stdio"]},
                {"type": "http", "name": "web", "url": "https://acp.example/mcp"}
            ]
        })),
    };
    let resp = block_on(adapter.handle(&req));
    let result = match resp {
        AcpResponse::Result { result, .. } => result,
        AcpResponse::Error { .. } => panic!("bound new_session must succeed"),
    };
    assert_eq!(
        result.get("sessionId").and_then(|v| v.as_str()),
        Some(bound_session.to_string().as_str()),
        "the reply carries the bound runner session id"
    );
    let seen = seen
        .lock()
        .expect("seen lock")
        .clone()
        .expect("binding ran");
    assert_eq!(seen.cwd, "/work/acp");
    assert_eq!(seen.additional_directories, vec!["/work/libs".to_string()]);
    // The stdio entry decodes without a type tag; the http entry keeps its
    // explicit one. Both shapes arrive typed, not as opaque JSON.
    match seen.mcp_servers.as_slice() {
        [McpServer::Stdio(stdio), McpServer::Http(http)] => {
            assert_eq!(stdio.command, "/usr/bin/fsmcp");
            assert_eq!(stdio.args, vec!["--stdio".to_string()]);
            assert_eq!(http.url, "https://acp.example/mcp");
        }
        other => panic!("mcp servers decoded wrong: {other:?}"),
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
    let resp = block_on(adapter.handle(&req));
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
    let resp = block_on(adapter.handle(&req));
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
    let resp = block_on(adapter.handle(&req));
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
/// activation step (lands when an ACP client reaches this adapter over a
/// persistent local transport).
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
                reason: None,
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
    let resp = block_on(adapter.handle(&req));
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
    let resp = block_on(adapter.handle(&req));
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
    let resp = block_on(adapter.handle(&req));
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
    block_on(adapter.handle_notification(&notif));
    assert_eq!(store.state(sid), LifecycleState::Cancelled);
}
