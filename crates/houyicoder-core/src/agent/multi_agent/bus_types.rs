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
}

/// The terminal status of a child agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChildStatus {
    Completed,
    Killed,
    Failed,
    DeadlineExceeded,
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
    }
}
