//! Frontend event types: the wire payload the engine sends the frontend
//! per streamed notification. Extracted from the frontend module root so
//! the root stays under the size gate.

use serde::{Deserialize, Serialize};

use super::memory::MemorySavedKind;
use super::session_update::SessionUpdate;

/// daemon -> frontend events (streaming notifications). Dedup by event id so
/// multi-agent output never duplicates on screen: several agents streaming at
/// once can deliver the same event twice, and the id is what makes the second
/// one droppable.
#[derive(Debug, Clone)]
pub struct FrontendEvent {
    pub id: String,
    pub kind: FrontendEventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum FrontendEventKind {
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
    /// The texts the runner drained from its mid-turn injection queue this
    /// run. Sent at run end so the frontend can remove them from its queue
    /// mirror. Reliable (durable, sent once per run before the outcome).
    QueueConsumed {
        texts: Vec<String>,
    },
    /// A background memory task (extract or dream) wrote the given count of
    /// entries this pass. Fired once per pass on completion, after the run
    /// ended. Best-effort (a full channel drops the notice; data is on disk).
    MemorySaved {
        count: u32,
        kind: MemorySavedKind,
    },
    /// A runtime notice the agent loop wants surfaced to the user as a system
    /// line (not a delta, not a tool frame). Carries pre-rendered text the
    /// host renders verbatim.
    SystemLine {
        text: String,
    },
    /// A spawned child's live status snapshot, for the agent status footer.
    /// The service fleet projector translates bus progress and completed
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
