//! Events emitted by the daemon to a connected frontend.

use serde::{Deserialize, Serialize};

use super::completion::CompletionStatus;
use super::memory::{MemoryChange, MemoryChangeCausality, MemoryChangeId, MemoryChangeOrigin};
use super::queue::QueuedInput;
use super::session_update::SessionUpdate;
use super::tasks::BackgroundTaskSnapshot;
use crate::acpx::AcpxNotification;

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
    /// the base protocol has a standard variant for. The service maps the
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
        notification: AcpxNotification,
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
        /// Which turn the changes belong to. A frame from a producer that
        /// predates the field still delivers its changes, read as an earlier
        /// turn.
        #[serde(default)]
        causality: MemoryChangeCausality,
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
    /// runs; Some once terminal, holding the ending the engine reported.
    AgentStatus {
        agent_id: String,
        subagent_type: String,
        /// The delegating call's task summary. Names what this child is
        /// doing, which the type alone cannot: two children of one type are
        /// otherwise indistinguishable in the fleet row. Empty for a spawn no
        /// model description reached (a hook- or gate-driven child).
        #[serde(default)]
        description: String,
        turn: u32,
        tokens: u64,
        tool_uses: u32,
        last_activity: Option<String>,
        completed: Option<CompletionStatus>,
    },
    /// One background command's latest snapshot. The frontend replaces the
    /// row it holds for that task number, so a frame lost in transit costs
    /// freshness rather than the task itself.
    BackgroundTaskChanged {
        task: BackgroundTaskSnapshot,
    },
    /// How many completion notifications wait for the parent model. An idle
    /// client reads this as the wake signal; the drain request consumes the
    /// whole queue, so a missed frame costs freshness, not an ending.
    NotificationsPending {
        count: u32,
    },
}

#[cfg(test)]
mod tests;
