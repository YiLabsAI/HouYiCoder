//! Tests for this module.

use super::*;
use houyicoder_async::CancellationToken;
use houyicoder_protocol::frontend::permission::{AskReason, AskSource};
use std::sync::Arc;
use tokio::task::yield_now;

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
    let decisions = route_approvals_via_bus(&bus, "child-1", "explore", approvals, &cancel)
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
        route_approvals_via_bus(&bus, "child-1", "explore", approvals, &cancel_for_task).await
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
    let decisions = route_approvals_via_bus(&bus, "child-d", "explore", approvals, &cancel)
        .await
        .expect("not canceled");
    assert_eq!(decisions.len(), 1);
    assert!(!decisions[0].approved);
    assert_eq!(decisions[0].call_id, "call-d");
}
