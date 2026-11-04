//! Tests for this module.

use super::*;
use houyicoder_api::spawn::PendingChildPermission;
use houyicoder_async::CancellationToken;
use houyicoder_protocol::frontend::permission::{AskReason, AskSource};
use std::sync::{Arc, Mutex};
use tokio::task::yield_now;

fn empty_pending() -> Arc<Mutex<Vec<PendingChildPermission>>> {
    Arc::new(Mutex::new(Vec::new()))
}

/// The park is cleared after the response: a reconnecting serve loop never
/// re-emits a settled ask.
#[tokio::test]
async fn test_route_approvals_clears_park() {
    let bus = Arc::new(AgentBus::new());
    let mut parent_rx = bus.subscribe(permission_request_topic());
    let bus_for_parent = Arc::clone(&bus);
    tokio::spawn(async move {
        let req = parent_rx.recv().await.expect("got the ask");
        let BusMessage::PermissionRequest {
            child_id, call_id, ..
        } = req
        else {
            panic!()
        };
        bus_for_parent.publish(
            &permission_response_topic(&child_id, &call_id),
            BusMessage::PermissionResponse {
                call_id,
                approved: true,
                updated_input: None,
                scope: "once".to_string(),
            },
        );
    });
    let cancel = CancellationToken::new();
    let pending = empty_pending();
    let approvals = vec![ApprovalRequest::new(
        "call-p".into(),
        "bash".into(),
        serde_json::json!({}),
        None,
    )];
    let decisions =
        route_approvals_via_bus(&bus, "child-p", "explore", approvals, &cancel, &pending)
            .await
            .expect("not canceled");
    assert_eq!(decisions.len(), 1);
    assert!(decisions[0].approved);
    assert!(
        pending.lock().map(|v| v.is_empty()).unwrap_or(true),
        "the park is cleared after the response"
    );
}

/// permission_request_from reconstructs the bus message a reconnecting serve
/// loop re-emits: every field the child parked travels through.
#[test]
fn test_permission_request_from_fields() {
    let ask = PendingChildPermission {
        child_id: "child-x".into(),
        subagent_type: "explore".into(),
        call_id: "call-x".into(),
        tool: "bash".into(),
        input: serde_json::json!({"command": "ls"}),
        reason: None,
    };
    let msg = permission_request_from(ask);
    let BusMessage::PermissionRequest {
        child_id,
        subagent_type,
        call_id,
        tool,
        input,
        reason,
    } = msg
    else {
        panic!("expected PermissionRequest");
    };
    assert_eq!(child_id, "child-x");
    assert_eq!(subagent_type, "explore");
    assert_eq!(call_id, "call-x");
    assert_eq!(tool, "bash");
    assert_eq!(input, serde_json::json!({"command": "ls"}));
    assert!(reason.is_none());
}

/// clear_pending removes the entry whose call_id matches and leaves others.
#[test]
fn test_clear_pending_removes_match() {
    let pending: Arc<Mutex<Vec<PendingChildPermission>>> = Arc::new(Mutex::new(vec![
        PendingChildPermission {
            child_id: "c1".into(),
            subagent_type: "explore".into(),
            call_id: "call-a".into(),
            tool: "bash".into(),
            input: serde_json::json!({}),
            reason: None,
        },
        PendingChildPermission {
            child_id: "c2".into(),
            subagent_type: "explore".into(),
            call_id: "call-b".into(),
            tool: "bash".into(),
            input: serde_json::json!({}),
            reason: None,
        },
    ]));
    super::clear_pending(&pending, "call-a");
    let v = pending.lock().expect("lock");
    assert_eq!(v.len(), 1, "only the matching entry is removed");
    assert_eq!(v[0].call_id, "call-b");
}

/// route_approvals_via_bus round-trips a permission ask to a parent
/// responder and collects the decision, with the child's cause travelling
/// on the message so the parent's card explains the call. drive_child_to_terminal
/// relies on subscribe-before-publish for the per-call response topic so no
/// broadcast lag drops the decision.
#[tokio::test]
async fn test_route_approvals_via_bus() {
    let bus = Arc::new(AgentBus::new());
    let mut parent_rx = bus.subscribe(permission_request_topic());
    let bus_for_parent = Arc::clone(&bus);
    let parent = tokio::spawn(async move {
        let req = parent_rx.recv().await.expect("parent got the ask");
        let BusMessage::PermissionRequest {
            child_id,
            call_id,
            reason,
            ..
        } = req
        else {
            panic!("expected PermissionRequest")
        };
        let carried = reason.expect("the child's cause travels with the ask");
        assert_eq!(carried.detail, "outside the workspace");
        bus_for_parent.publish(
            &permission_response_topic(&child_id, &call_id),
            BusMessage::PermissionResponse {
                call_id,
                approved: true,
                updated_input: None,
                scope: "once".to_string(),
            },
        );
    });
    let cancel = CancellationToken::new();
    let reason = AskReason {
        source: AskSource::Detection,
        validator: "path-bounds".to_string(),
        detail: "outside the workspace".to_string(),
        containment_note: None,
    };
    let approvals = vec![ApprovalRequest::new(
        "call-1".into(),
        "bash".into(),
        serde_json::json!({"command": "echo hi"}),
        Some(reason),
    )];
    let decisions = route_approvals_via_bus(
        &bus,
        "child-1",
        "explore",
        approvals,
        &cancel,
        &empty_pending(),
    )
    .await
    .expect("not canceled");
    assert_eq!(decisions.len(), 1);
    assert!(decisions[0].approved);
    assert_eq!(decisions[0].call_id, "call-1");
    parent.await.expect("parent responder done");
}

/// A canceled route returns None rather than hanging on a response the
/// parent will not send.
#[tokio::test]
async fn test_route_approvals_cancel_none() {
    let bus = Arc::new(AgentBus::new());
    let cancel = CancellationToken::new();
    let approvals = vec![ApprovalRequest::new(
        "call-1".into(),
        "bash".into(),
        serde_json::json!({}),
        None,
    )];
    let cancel_for_task = cancel.clone();
    let task = tokio::spawn(async move {
        route_approvals_via_bus(
            &bus,
            "child-1",
            "explore",
            approvals,
            &cancel_for_task,
            &empty_pending(),
        )
        .await
    });
    yield_now().await;
    cancel.cancel();
    assert!(
        task.await.expect("task done").is_none(),
        "canceled route returns None, not a hang"
    );
}

/// A denied verdict reaches the child as a reject decision. The timeout arm
/// pushes the same shape (approved=false), so this covers the deny path the
/// timeout takes.
#[tokio::test]
async fn test_route_approvals_deny() {
    let bus = Arc::new(AgentBus::new());
    let mut parent_rx = bus.subscribe(permission_request_topic());
    let bus_for_parent = Arc::clone(&bus);
    tokio::spawn(async move {
        let req = parent_rx.recv().await.expect("got the ask");
        let BusMessage::PermissionRequest {
            child_id, call_id, ..
        } = req
        else {
            panic!()
        };
        bus_for_parent.publish(
            &permission_response_topic(&child_id, &call_id),
            BusMessage::PermissionResponse {
                call_id,
                approved: false,
                updated_input: None,
                scope: "once".to_string(),
            },
        );
    });
    let cancel = CancellationToken::new();
    let approvals = vec![ApprovalRequest::new(
        "call-d".into(),
        "bash".into(),
        serde_json::json!({}),
        None,
    )];
    let decisions = route_approvals_via_bus(
        &bus,
        "child-d",
        "explore",
        approvals,
        &cancel,
        &empty_pending(),
    )
    .await
    .expect("not canceled");
    assert_eq!(decisions.len(), 1);
    assert!(!decisions[0].approved);
    assert_eq!(decisions[0].call_id, "call-d");
}
