//! Events emitted by the daemon to a connected frontend.

use serde::{Deserialize, Serialize};

use super::memory::{MemoryChange, MemoryChangeId, MemoryChangeOrigin};
use super::queue::QueuedInput;
use super::session_update::SessionUpdate;

/// One event emitted by the daemon to a connected frontend.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum FrontendEvent {
    Message {
        delta: String,
    },
    Diff {
        path: String,
        patch: String,
    },
    ToolProgress {
        name: String,
        status: String,
    },
    PermissionAsk {
        reason: String,
    },
    /// A multi-agent fleet event (finding / verdict / progress). Routed via
    /// the A2A bus; deduped by id so no duplicate output.
    AgentEvent {
        topic: String,
        summary: String,
    },
    Metrics {
        tokens: u64,
        cache_hit_ratio: f32,
    },
    /// Spec/plan artifact produced by the guided flow.
    Artifact {
        kind: String,
        id: String,
    },
    /// Live spec-vs-impl divergence for one clause. status is one of
    /// unimplemented / partial / satisfied (stub string until typed).
    SpecImplDivergence {
        clause_id: String,
        status: String,
    },
    /// A review finding arrived from the multi-agent adversarial review and is
    /// awaiting human sign-off (review-node console).
    FindingArrived {
        finding_id: String,
    },
    /// One ACP session/update notification, the typed form of a turn event
    /// the base protocol has a standard variant for. The service projects the
    /// engine turn event to this wire type at the boundary; the frontend
    /// renders the turn stream without importing engine types.
    SessionUpdate {
        update: SessionUpdate,
    },
    /// An acpx/* extension notification: a turn-event kind the base protocol
    /// has no standard variant for, or a token-level LlmEvent the provider
    /// streams. The method string travels on the wire; the typed AcpxMethod
    /// lives in crate::acpx so the string never leaks past the adapter.
    Acpx {
        notification: crate::acpx::AcpxNotification,
    },
    /// Queued inputs durably committed to the session at a turn boundary.
    #[serde(rename = "QueueConsumed")]
    QueuedInputCommitted {
        #[serde(rename = "texts")]
        inputs: Vec<QueuedInput>,
    },
    /// Successful memory changes emitted together by one producer.
    MemoryChanged {
        /// Unique delivery identity.
        id: MemoryChangeId,
        /// Producer responsible for the changes.
        origin: MemoryChangeOrigin,
        /// Exact successful operations in append order.
        changes: Vec<MemoryChange>,
    },
    /// A runtime notice the agent loop wants surfaced to the user as a system
    /// line (not a delta, not a tool frame). Carries pre-rendered text the
    /// host renders verbatim.
    SystemLine {
        text: String,
    },
    /// A spawned child's live status snapshot, for the agent status footer.
    /// The service fleet status relay translates bus progress and completed
    /// messages into this wire frame so the frontend renders the footer
    /// without touching the engine bus. completed is None while the child
    /// runs; Some once terminal ("completed" / "failed" / "killed" / ...).
    AgentStatus {
        agent_id: String,
        subagent_type: String,
        turn: u32,
        tokens: u64,
        tool_uses: u32,
        last_activity: Option<String>,
        completed: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontend::memory::MemoryOperation;

    #[test]
    fn test_memory_event_round_trips() {
        let event = FrontendEvent::MemoryChanged {
            id: MemoryChangeId("change-1".into()),
            origin: MemoryChangeOrigin::AutoMemory,
            changes: vec![MemoryChange {
                key: "build-gate".into(),
                operation: MemoryOperation::Stored,
            }],
        };
        let json = serde_json::to_string(&event).expect("serialize memory event");
        let decoded = serde_json::from_str::<FrontendEvent>(&json).expect("decode memory event");
        assert!(matches!(
            decoded,
            FrontendEvent::MemoryChanged { id, changes, .. }
                if id.0 == "change-1" && changes[0].key == "build-gate"
        ));
    }

    #[test]
    fn test_commit_wire_compat() {
        let event = FrontendEvent::QueuedInputCommitted {
            inputs: vec![QueuedInput::new("next")],
        };
        let value = serde_json::to_value(&event).unwrap();
        assert!(value.get("QueueConsumed").is_some());
        assert!(value["QueueConsumed"].get("texts").is_some());

        let legacy = serde_json::json!({"QueueConsumed": {"texts": ["next"]}});
        let decoded: FrontendEvent = serde_json::from_value(legacy).unwrap();
        let FrontendEvent::QueuedInputCommitted { inputs } = decoded else {
            panic!("queued input commit");
        };
        assert_eq!(inputs[0].text, "next");
    }
}
