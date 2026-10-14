//! Tests for this module.

use super::*;
use houyicoder_async::bus::MessageBus;

#[test]
fn test_agent_bus_pub_sub() {
    let bus = AgentBus::new();
    let mut rx = bus.subscribe(&progress_topic("child-1"));
    bus.publish(
        &progress_topic("child-1"),
        BusMessage::Progress {
            child: ChildDescriptor {
                agent_id: "child-1".into(),
                agent_type: "explore".into(),
                run_mode: ChildRunMode::Foreground,
            },
            turn: 1,
            tokens: 100,
            tool_uses: 2,
            last_activity: Some("reading".into()),
        },
    );
    match rx.try_recv().expect("message received") {
        BusMessage::Progress { child, turn, .. } => {
            assert_eq!(child.agent_id, "child-1");
            assert_eq!(turn, 1);
        }
        _ => panic!("expected Progress"),
    }
}

#[test]
fn test_topic_helpers() {
    assert_eq!(progress_topic("abc"), "task.abc.progress");
    assert_eq!(completed_topic("abc"), "task.abc.completed");
    assert_eq!(global_progress_topic(), "agents.progress");
    assert_eq!(global_completed_topic(), "agents.completed");
    assert_eq!(permission_request_topic(), "agents.permission_request");
    assert_eq!(
        permission_response_topic("c1", "call-9"),
        "task.c1.permission_response.call-9"
    );
}

/// The child subscribes before publishing its request so the response
/// cannot be lost to broadcast lag.
#[tokio::test]
async fn test_child_receives_permission_reply() {
    let bus = AgentBus::new();
    let mut parent_rx = bus.subscribe(permission_request_topic());
    let child_id = "child-1";
    let call_id = "call-1";
    // Child subscribes to its response topic BEFORE publishing.
    let mut resp_rx = bus.subscribe(&permission_response_topic(child_id, call_id));
    bus.publish(
        permission_request_topic(),
        BusMessage::PermissionRequest {
            child_id: child_id.into(),
            subagent_type: "explore".into(),
            call_id: call_id.into(),
            tool: "bash".into(),
            input: serde_json::json!({"command": "rm -rf x"}),
        },
    );
    // Parent receives the ask.
    match parent_rx.try_recv().expect("parent got the request") {
        BusMessage::PermissionRequest {
            child_id: cid,
            tool,
            ..
        } => {
            assert_eq!(cid, child_id);
            assert_eq!(tool, "bash");
        }
        other => panic!("expected PermissionRequest, got {other:?}"),
    }
    // Parent publishes the decision on the per-request response topic.
    bus.publish(
        &permission_response_topic(child_id, call_id),
        BusMessage::PermissionResponse {
            call_id: call_id.into(),
            approved: false,
            updated_input: None,
            scope: "once".into(),
        },
    );
    // Child receives its decision (no lag: subscribed before publish).
    match resp_rx.recv().await.expect("child got the decision") {
        BusMessage::PermissionResponse { approved, .. } => assert!(!approved),
        other => panic!("expected PermissionResponse, got {other:?}"),
    }
}
