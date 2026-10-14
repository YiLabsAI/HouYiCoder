//! Wire shapes for a run outcome and the approval handshake. The engine's
//! RunResult / RunOutcome / RunError are Debug-only types carrying complex
//! provider, context, and verify-failure payloads; they do not cross the
//! wire directly. The service maps them to these serde shapes at the
//! boundary; the client deserializes them so the frontend branches on the
//! same outcome shape the engine produced, without depending on engine
//! types.

use crate::acp_wire::PermissionOption;
use crate::frontend::permission::AskReason;
use crate::llm::Usage;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The reason a run stopped, wire form. A closed string enum matching the
/// base agent protocol prompt stop_reason field so a standard client can
/// decode every value a peer emits (the cross-decode fixture gates this).
/// The service emits only EndTurn / Cancelled / MaxTurnRequests today;
/// MaxTokens and Refusal are kept for cross-decode parity and are not
/// emitted yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
}

/// One content block of a multimodal message, wire form. Internally tagged
/// by type to match the base agent protocol content block shape
/// ({"type":"text","text":"..."}) so the cross-decode fixture passes. Text
/// carries the string; Image carries base64 data plus its mime type (not a
/// URL + alt text). The mime_type field renames to mimeType on the wire to
/// match the base agent protocol ImageContent (camelCase); the enum-level
/// snake_case applies to variant tags (text / image), and the field rename
/// overrides it for this one camelCase field. non_exhaustive so an Audio or
/// future block lands without reworking every match.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
}

/// A wire approval request: the tool call id, the tool name, and the input
/// the model passed. The ToolCall event is already in the session
/// log; approval decides whether resume executes it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub call_id: String,
    pub tool_name: String,
    pub input: Value,
    /// The verdict options the server offers (N-option: the card renders
    /// these dynamically, not a hardcoded set). Empty when the server
    /// defers the option set to the client; the client uses its default.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<PermissionOption>,
    /// Why the engine is asking. None when an older engine that does not
    /// attach a reason sent the request — the card renders a generic prompt.
    /// Default + skip-when-none keep the field backward compatible both ways:
    /// an old client ignores it, an old server omits it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<AskReason>,
    /// The child delegation this ask was routed up from over the bus, when
    /// the ask originates from a spawned child rather than the parent's own
    /// run. None for a parent tool call. The card labels the child type so a
    /// user can tell a child's ask from the parent's when several delegations
    /// are in flight. Default + skip-when-none keep the field backward
    /// compatible both ways.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation: Option<DelegationSource>,
}

/// The origin of a permission ask routed up from a spawned child: the child
/// session id and its agent type, so the approval card can label which
/// delegation the ask belongs to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegationSource {
    pub child_id: String,
    pub subagent_type: String,
}

/// A caller decision on one approval. A rejected call gets a rejected-by-user
/// tool result so the model sees the veto; updated_input carries an answer-
/// populated input the human-in-the-loop UI injected (None for a plain
/// approve or reject). scope is the consent breadth the client chose (once /
/// prefix / session); the server records it on the durable PermissionDecision
/// audit event so the verdict trail survives the wire path (the engine
/// ApprovalDecision carries no scope — it is consumed at the boundary).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalDecision {
    pub call_id: String,
    pub approved: bool,
    pub updated_input: Option<Value>,
    pub scope: String,
}

/// How a run ended, wire form. The engine RunOutcome carries a VerifyFailure
/// and an AgentId; here VerifyFailed carries a summary string the frontend
/// surfaces, and Handoff carries the agent id as a string. Tagged so the
/// frontend matches one arm per outcome, matching the engine enum.
///
/// Interruption is not a wire outcome: a tool needing approval is surfaced
/// mid-turn as a server-to-client reverse request (ServerRequestEnvelope with
/// a Permission payload); the run does not end. The service drives the
/// suspend-return + reverse-request + resume loop; the wire only carries the
/// final outcome when the run actually ends.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[non_exhaustive]
pub enum RunOutcome {
    /// The model produced a final answer, as one or more content blocks.
    FinalOutput { content: Vec<ContentBlock> },
    /// The turn handed off to another agent; the caller spawns it.
    Handoff { agent: String },
    /// An external abort cancelled an in-flight run. The string carries the
    /// reason.
    Interrupted { reason: String },
    /// FinalOutput reached but the verify gate rejected it. The string is the
    /// finding summary the frontend surfaces before re-prompting the model.
    VerifyFailed { summary: String },
    /// The run hit the max_turns backstop. The model did not produce a final
    /// answer; the caller can resume to continue. Carries is_error semantics
    /// (a graceful result, not a crash) — the turns + usage still travel the
    /// RunResult so the frontend shows cost statistics.
    MaxTurnsReached { turns: u32 },
}

/// A run result, wire form. Carries the outcome, the turn count, the token
/// usage, and the stop reason mapped from the engine outcome at the service
/// boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunResult {
    pub outcome: RunOutcome,
    pub turns: u32,
    pub usage: Usage,
    pub stop_reason: StopReason,
}

/// A run failure, wire form. The engine RunError carries ContextError and
/// ProviderError; here it is the category plus the Display string the
/// frontend records as an error line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunError {
    pub category: String,
    pub message: String,
}

#[cfg(test)]
mod tests;
