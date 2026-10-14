//! Domain message types + topic conventions for the multi-agent bus.
//!
//! The transport (MessageBus trait + InProcBus) is generic and lives in
//! the async foundation crate; this module defines the agent-domain T
//! that plugs into it: BusMessage, ChildStatus, and the topic naming scheme.

use houyicoder_async::bus::InProcBus;
use serde::{Deserialize, Serialize};

/// Whether a child blocks its parent turn or continues independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChildRunMode {
    /// The parent waits for the child result.
    Foreground,
    /// The child continues after the spawn call returns.
    Background,
}

/// Stable identity and execution mode for a child agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildDescriptor {
    /// Child session identity.
    pub agent_id: String,
    /// Registered child-agent type.
    pub agent_type: String,
    /// Product execution mode.
    pub run_mode: ChildRunMode,
}

impl ChildDescriptor {
    /// Describe one child agent and its run mode.
    pub fn new(
        agent_id: impl Into<String>,
        agent_type: impl Into<String>,
        run_mode: ChildRunMode,
    ) -> Self {
        Self {
            agent_id: agent_id.into(),
            agent_type: agent_type.into(),
            run_mode,
        }
    }
}

/// A message carried on the agent bus. Enumerated so subscribers match
/// on kind without parsing strings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BusMessage {
    /// Per-turn progress from a running child.
    Progress {
        child: ChildDescriptor,
        turn: u32,
        tokens: u64,
        tool_uses: u32,
        last_activity: Option<String>,
    },
    /// Terminal state with a self-contained child descriptor.
    Completed {
        child: ChildDescriptor,
        status: ChildStatus,
        summary: String,
    },
    /// A message delivered to a child's inbox (parent -> child).
    Inbox { text: String },
    /// Announced before the child's first status event.
    Spawned { child: ChildDescriptor },
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
pub type AgentBus = InProcBus<BusMessage>;

/// Build a topic string for a child's progress channel. Per-child targeted
/// channel for a watcher that already learned the id from Spawned. The
/// status publisher also fans Progress out to the global progress topic so
/// a watcher subscribed before any child spawns cannot lose a one-shot
/// message to the subscribe-after-publish race.
pub fn progress_topic(agent_id: &str) -> String {
    format!("task.{agent_id}.progress")
}

/// Build a topic string for a child's completion channel. Per-child targeted
/// channel for a watcher that already learned the id from Spawned. The
/// status publisher also fans Completed out to the global completion topic
/// so a watcher subscribed before any child spawns cannot lose the terminal
/// event: a per-child subscribe that lands after the child already completed
/// would miss the one-shot Completed entirely (broadcast does not replay
/// history), leaving the parent notification lost and the footer strip stuck
/// on running.
pub fn completed_topic(agent_id: &str) -> String {
    format!("task.{agent_id}.completed")
}

/// The global progress topic for watchers subscribed before children spawn.
pub fn global_progress_topic() -> &'static str {
    "agents.progress"
}

/// The global completion topic: a watcher subscribes once at startup (before
/// any child spawns) and demuxes by the agent_id each Completed carries.
/// Subscribe-before-publish guarantees the terminal event is never lost to a
/// per-child subscribe landing after the child already completed. The status
/// publisher fans out to this + the per-child channel.
pub fn global_completed_topic() -> &'static str {
    "agents.completed"
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
mod tests;
