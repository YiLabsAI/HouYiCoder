//! The acpx/* extension surface: the protocol's own ACP-shaped extension
//! methods (serde wire types matching, no dependency on the
//! agent-client-protocol crate, which stays confined to the service layer). A client
//! opts into acpx via a capability flag at Hello; a standard client ignores
//! the unknown methods (JSON-RPC 2.0 permits), so a pure-base client is a
//! drop-in.
//!
//! The method string travels on the wire (the base protocol's ext_*
//! mechanism is string-keyed by design); the typed AcpxMethod enum lives
//! here so the string never leaks past the adapter boundary — every consumer
//! matches the typed enum, so a method this build does not know decodes as
//! Unknown instead of failing the read. The adapter (service layer) maps the
//! wire string to the enum on the way in and back on the way out.
//!
//! LlmEvent (token-level provider stream) maps onto acpx/llm/* as an
//! independent notification stream — it does NOT ride the base session/update
//! channel, which carries the turn-level SessionLogEntry mapping. The two are
//! orthogonal: LlmEvent is live token flow; SessionLogEntry is the durable turn
//! record.

use crate::llm::LlmEvent;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The typed acpx/* method namespace. Wire-serializes to the string key the
/// base protocol ext_* mechanism carries (e.g. LlmTextDelta serializes as
/// "acpx/llm/text_delta"). non_exhaustive so a new extension method lands
/// without reworking every match; a method this build does not know decodes
/// as Unknown, so a newer peer's addition costs one skipped notification
/// instead of the connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum AcpxMethod {
    // acpx/llm/* — the token-level provider stream (matches LlmEvent).
    #[serde(rename = "acpx/llm/step_start")]
    LlmStepStart,
    #[serde(rename = "acpx/llm/step_finish")]
    LlmStepFinish,
    #[serde(rename = "acpx/llm/text_start")]
    LlmTextStart,
    #[serde(rename = "acpx/llm/text_delta")]
    LlmTextDelta,
    #[serde(rename = "acpx/llm/text_end")]
    LlmTextEnd,
    #[serde(rename = "acpx/llm/reasoning_start")]
    LlmReasoningStart,
    #[serde(rename = "acpx/llm/reasoning_delta")]
    LlmReasoningDelta,
    #[serde(rename = "acpx/llm/reasoning_end")]
    LlmReasoningEnd,
    #[serde(rename = "acpx/llm/tool_input_start")]
    LlmToolInputStart,
    #[serde(rename = "acpx/llm/tool_input_delta")]
    LlmToolInputDelta,
    #[serde(rename = "acpx/llm/tool_input_end")]
    LlmToolInputEnd,
    #[serde(rename = "acpx/llm/tool_call")]
    LlmToolCall,
    #[serde(rename = "acpx/llm/tool_result")]
    LlmToolResult,
    #[serde(rename = "acpx/llm/tool_error")]
    LlmToolError,
    #[serde(rename = "acpx/llm/finish")]
    LlmFinish,
    #[serde(rename = "acpx/llm/provider_error")]
    LlmProviderError,

    // acpx/max_turns — the MaxTurnsReached side channel (the run hit the
    // turn cap; the stop_reason carries max_turn_requests, the turn count
    // rides here). Lands when the reverse-request mapping promotes it.
    #[serde(rename = "acpx/max_turns")]
    MaxTurns,

    // acpx/tool/progress — a long-running tool (currently bash) reports its
    // elapsed seconds so the host can show the chip is not stuck. Carries
    // call_id + elapsed_secs in params. Ephemeral like the llm deltas: a
    // later authoritative tool-result frame supersedes it.
    #[serde(rename = "acpx/tool/progress")]
    ToolProgress,

    // acpx/context/* — SessionLogEntry kinds the base session/update has no
    // standard counterpart for (CompactionBoundary, Summary, MetaUser,
    // PermissionDecision, RunCompleted). These ride the extension
    // notification stream (the durable-context audit trail), orthogonal to
    // session/update.
    #[serde(rename = "acpx/context/compaction_boundary")]
    ContextCompactionBoundary,
    #[serde(rename = "acpx/context/summary")]
    ContextSummary,
    #[serde(rename = "acpx/context/meta_user")]
    ContextMetaUser,
    #[serde(rename = "acpx/context/permission_decision")]
    ContextPermissionDecision,
    /// A run reached its terminal outcome. The params carry the turn's
    /// duration in ms, the sum of how long its drive legs ran, which is the
    /// work it spent rather than the wall clock it was open for. The host
    /// closes the turn's summary row on this frame, so a replayed session
    /// shows the same row the live one did.
    #[serde(rename = "acpx/context/run_completed")]
    ContextRunCompleted,
    /// A message delivered into a running turn rather than one that opens it.
    /// The message itself rides the user-message stream; this mark rides
    /// beside it, because the chunk a client reads cannot say which of the two
    /// it is and a turn boundary turns on that. A queued interjection and a
    /// background child's completion are both delivered this way.
    #[serde(rename = "acpx/context/mid_turn_input")]
    ContextMidTurnInput,
    /// A background child finished and its result was handed to the running
    /// turn. Marked as delivered rather than opening, by the same rule as a
    /// queued interjection.
    #[serde(rename = "acpx/context/child_completed")]
    ContextChildCompleted,
    /// A turn was interrupted (a cancel, or a process that died mid-turn) and
    /// the run regenerates inside the same user turn. The notice rides the
    /// user-message stream and this mark rides beside it, so a reader does not
    /// mistake the notice for a message that opens a turn and split one user
    /// turn into two summary rows.
    #[serde(rename = "acpx/context/turn_interrupted")]
    ContextTurnInterrupted,

    // acpx/a2a/*, acpx/trajectory/*, acpx/cas/* — placeholder namespaces;
    // payload shapes land with their respective subsystems.
    #[serde(rename = "acpx/a2a/handoff")]
    A2aHandoff,
    #[serde(rename = "acpx/trajectory/snapshot")]
    TrajectorySnapshot,
    #[serde(rename = "acpx/cas/block_ref")]
    CasBlockRef,

    // acpx/session/takeControl — a session-scoped ext_method request (not a
    // notification): the client asks to take the control lease for a session,
    // optionally forcing (cancel the live turn + take the lease). The adapter
    // resolves the pending reverse request future and replies with a
    // TakeControlOutcome. Rides the ext_method request axis (carries a
    // req_id), unlike the notification methods above which have no req_id.
    #[serde(rename = "acpx/session/takeControl")]
    SessionTakeControl,

    /// A method this build does not know: a newer peer's addition, or a name
    /// this build writes wrong (a rename that drifted from its serde name).
    /// Either way it arrives here rather than failing the read that carried
    /// it. The name is not retained; a consumer reporting what arrived reads
    /// the params. This variant serializes as the literal Unknown, which no
    /// peer defines as a method.
    #[serde(other)]
    Unknown,
}

/// One acpx extension notification. The shape matches a base-protocol
/// ext_* notification: a method string (typed here) plus a params object the
/// method dictates. The adapter wraps this in the base notification envelope;
/// here it is the typed payload only.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpxNotification {
    pub method: AcpxMethod,
    /// The method's own fields. JSON-RPC 2.0 lets a notification omit params,
    /// so an absent one reads as null here: the method still decodes, and a
    /// reader that looks a field up gets null rather than a failed read.
    #[serde(default)]
    pub params: Value,
}

impl AcpxNotification {
    pub fn new(method: AcpxMethod, params: Value) -> Self {
        Self { method, params }
    }
}

/// The typed capability block the adapter places at initialize-response
/// _meta.acpx. A pure-ACP client ignores _meta (unknown field); an
/// acpx-aware client reads it to learn streaming/cas/detach support and the
/// ext_method verbs the agent answers. Declaring capabilities here (not via
/// an ext_method probe) is atomic with the handshake, so a reconnecting
/// client knows lease/detach support before loadSession resends a pending
/// ask — there is no window between handshake and a probe reply.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpxCapabilities {
    /// True when the agent streams token-level acpx/llm/* notifications (the
    /// live-preview path). A pure-ACP client sees session/update only.
    #[serde(default)]
    pub streaming: bool,
    /// True when the agent supports content-addressed storage block
    /// retrieval (acpx/cas/*).
    #[serde(default)]
    pub cas: bool,
    /// True when the agent supports session detach + reattach (the
    /// control-lease lifecycle: loadSession + takeControl).
    #[serde(default)]
    pub detach: bool,
    /// The acpx/session/* ext_method request verbs the agent answers. A
    /// client probing an unlisted verb gets method-not-found, so this list
    /// is the probe-free discovery surface.
    #[serde(default)]
    pub ext_methods: Vec<String>,
}

/// Params for the acpx/session/takeControl ext_method request. session_id
/// identifies the session whose lease the caller wants; force = cancel the
/// live turn (the pending permission resolves to Cancelled) and take the
/// lease; without force the request waits for the pending prompt to finish.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TakeControlParams {
    pub session_id: String,
    #[serde(default)]
    pub force: bool,
}

/// The outcome the adapter returns for acpx/session/takeControl. Granted
/// carries pending_resent so the new holder knows whether a pending
/// permission ask was re-sent to it (the client surfaces the card). Denied
/// carries a reason (e.g. force unavailable, no such session). When force is
/// set, the adapter resolves to Granted only after the turn is cancelled and
/// the pending ask reaped — never while the old turn is live.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TakeControlOutcome {
    Granted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pending_resent: Option<bool>,
    },
    Denied {
        reason: String,
    },
}

/// Map a token-level LlmEvent to its acpx/llm/* notification. The
/// method is keyed off the variant; the params carry the event's own fields
/// (serialized as the event's serde shape) so a client reconstructs the same
/// typed event. A future variant with no method mapping returns None so the
/// adapter drops it rather than inventing a key.
pub fn map_llm_event(event: &LlmEvent) -> Option<AcpxNotification> {
    let method = match event {
        LlmEvent::StepStart { .. } => AcpxMethod::LlmStepStart,
        LlmEvent::StepFinish { .. } => AcpxMethod::LlmStepFinish,
        LlmEvent::TextStart { .. } => AcpxMethod::LlmTextStart,
        LlmEvent::TextDelta { .. } => AcpxMethod::LlmTextDelta,
        LlmEvent::TextEnd { .. } => AcpxMethod::LlmTextEnd,
        LlmEvent::ReasoningStart { .. } => AcpxMethod::LlmReasoningStart,
        LlmEvent::ReasoningDelta { .. } => AcpxMethod::LlmReasoningDelta,
        LlmEvent::ReasoningEnd { .. } => AcpxMethod::LlmReasoningEnd,
        LlmEvent::ToolInputStart { .. } => AcpxMethod::LlmToolInputStart,
        LlmEvent::ToolInputDelta { .. } => AcpxMethod::LlmToolInputDelta,
        LlmEvent::ToolInputEnd { .. } => AcpxMethod::LlmToolInputEnd,
        LlmEvent::ToolCall { .. } => AcpxMethod::LlmToolCall,
        LlmEvent::ToolResult { .. } => AcpxMethod::LlmToolResult,
        LlmEvent::ToolError { .. } => AcpxMethod::LlmToolError,
        LlmEvent::Finish { .. } => AcpxMethod::LlmFinish,
        LlmEvent::ProviderError { .. } => AcpxMethod::LlmProviderError,
    };
    let params = serde_json::to_value(event).ok()?;
    Some(AcpxNotification::new(method, params))
}

#[cfg(test)]
mod tests;
