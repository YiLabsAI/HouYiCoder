//! Tests for this module.

use super::*;
use houyicoder_async::CancellationToken;
use std::sync::Arc;

/// route_approvals_via_bus round-trips a permission ask to a parent
/// responder and collects the decision. drive_child_to_terminal relies
/// on subscribe-before-publish for the per-call response topic so no
/// broadcast lag drops the decision.
#[tokio::test]
async fn test_route_approvals_via_bus() {
    let bus = Arc::new(AgentBus::new());
    let mut parent_rx = bus.subscribe(permission_request_topic());
    let bus_for_parent = Arc::clone(&bus);
    let parent = tokio::spawn(async move {
        let req = parent_rx.recv().await.expect("parent got the ask");
        let BusMessage::PermissionRequest {
            child_id, call_id, ..
        } = req
        else {
            panic!("expected PermissionRequest")
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
    let approvals = vec![ApprovalRequest::new(
        "call-1".into(),
        "bash".into(),
        serde_json::json!({"command": "echo hi"}),
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
    )];
    let cancel_for_task = cancel.clone();
    let task = tokio::spawn(async move {
        route_approvals_via_bus(&bus, "child-1", "explore", approvals, &cancel_for_task).await
    });
    tokio::task::yield_now().await;
    cancel.cancel();
    assert!(
        task.await.expect("task done").is_none(),
        "canceled route returns None, not a hang"
    );
}
