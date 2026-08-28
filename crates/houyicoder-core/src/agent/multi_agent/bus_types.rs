//! Domain message types + topic conventions for the multi-agent bus.
//!
//! The transport (MessageBus trait + InProcBus) is generic and lives in
//! the async foundation crate; this module defines the agent-domain T
//! that plugs into it: BusMessage, ChildStatus, and the topic naming scheme.

use serde::{Deserialize, Serialize};

/// A message carried on the agent bus. Enumerated so subscribers match
/// on kind without parsing strings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BusMessage {
    /// Per-turn progress from a running child.
    Progress {
        agent_id: String,
        turn: u32,
        tokens: u64,
        tool_uses: u32,
        last_activity: Option<String>,
    },
    /// Terminal state: the child is done (or failed, or was killed).
    Completed {
        agent_id: String,
        status: ChildStatus,
        summary: String,
    },
    /// A message delivered to a child's inbox (parent -> child).
    Inbox { text: String },
    /// Announced when a child spawns so a watcher can subscribe to that
    /// child's progress and completed topics before the first turn lands. The
    /// run_in_background flag marks detached (async) spawns: a completion
    /// notification injector subscribes only to those, so a sync child — whose
    /// result the parent already receives as the tool result — does not get a
    /// redundant "child completed" notification re-injected into its own turn.
    Spawned {
        agent_id: String,
        subagent_type: String,
        run_in_background: bool,
    },
    /// A child asks the parent to approve a guarded tool call. Published on
    /// the global permission-request topic; the parent server subscribes,
    /// surfaces the ask through its existing wire-approval flow, and
    /// publishes a PermissionResponse on the per-request response topic.
    PermissionRequest {
        child_id: String,
        subagent_type: String,
        call_id: String,
        tool: String,
        input: serde_json::Value,
    },
    /// The parent's decision for a child's permission ask. Published on the
    /// per-request response topic the child subscribed to before it
    /// published the request, so no broadcast lag is possible.
    PermissionResponse {
        call_id: String,
        approved: bool,
        updated_input: Option<serde_json::Value>,
        scope: String,
    },
}

/// The terminal status of a child agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChildStatus {
    Completed,
    Killed,
    Failed,
    /// The child hit its max-turns cap (TurnLimit): it did partial work then
    /// stopped at the turn limit. Distinct from Deadline (a wall-clock
    /// timeout, a future terminal) and from Killed (an external cancel).
    TurnLimit,
    BudgetExhausted,
}

/// The agent bus type alias: InProcBus instantiated with the domain
/// message type.
pub type AgentBus = houyicoder_async::bus::InProcBus<BusMessage>;

/// Build a topic string for a child's progress channel.
pub fn progress_topic(agent_id: &str) -> String {
    format!("task.{agent_id}.progress")
}

/// Build a topic string for a child's completion channel.
pub fn completed_topic(agent_id: &str) -> String {
    format!("task.{agent_id}.completed")
}

/// The global topic a watcher subscribes to before any child spawns: the
/// runtime publishes Spawned here so the watcher learns each new child's id
/// and type and can subscribe to that child's progress and completed topics
/// before the first turn lands.
pub fn spawned_topic() -> &'static str {
    "agents.spawned"
}

/// The global topic the parent server subscribes to for child permission
/// asks. A child publishes a PermissionRequest here; the parent routes it
/// through its wire-approval flow and publishes the PermissionResponse on
/// the per-request response topic.
pub fn permission_request_topic() -> &'static str {
    "agents.permission_request"
}

/// The per-request topic a child subscribes to BEFORE publishing its
/// PermissionRequest. Subscribe-before-publish guarantees the child's
/// receiver exists when the parent later publishes the PermissionResponse,
/// so no broadcast lag can drop the decision.
pub fn permission_response_topic(child_id: &str, call_id: &str) -> String {
    format!("task.{child_id}.permission_response.{call_id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_async::bus::MessageBus;

    #[test]
    fn test_agent_bus_pub_sub() {
        let bus = AgentBus::new();
        let mut rx = bus.subscribe(&progress_topic("child-1"));
        bus.publish(
            &progress_topic("child-1"),
            BusMessage::Progress {
                agent_id: "child-1".into(),
                turn: 1,
                tokens: 100,
                tool_uses: 2,
                last_activity: Some("reading".into()),
            },
        );
        match rx.try_recv().expect("message received") {
            BusMessage::Progress { agent_id, turn, .. } => {
                assert_eq!(agent_id, "child-1");
                assert_eq!(turn, 1);
            }
            _ => panic!("expected Progress"),
        }
    }

    #[test]
    fn test_topic_helpers() {
        assert_eq!(progress_topic("abc"), "task.abc.progress");
        assert_eq!(completed_topic("abc"), "task.abc.completed");
        assert_eq!(permission_request_topic(), "agents.permission_request");
        assert_eq!(
            permission_response_topic("c1", "call-9"),
            "task.c1.permission_response.call-9"
        );
    }

    /// The permission round-trip: a child subscribes to its per-request
    /// response topic BEFORE publishing the request, so the parent's later
    /// response cannot be lost to broadcast lag. Pins the ordering contract
    /// the run-loop relies on.
    #[tokio::test]
    async fn test_permission_request_response_roundtrip() {
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
}
