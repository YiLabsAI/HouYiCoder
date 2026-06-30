//! The reverse permission ask: mid-turn, a tool needs a human verdict before
//! the run proceeds. The adapter sends a RequestPermissionRequest carrying
//! the call under review and the options the client may return; the client
//! picks one (allow once / always, reject once / always) or the ask is
//! cancelled. A session/cancel reaps a pending ask to Cancelled rather than
//! leaving a dangling JSON-RPC id.

use serde::{Deserialize, Serialize};

use super::Meta;

/// The kind of a permission option a client may pick: allow/reject, once or
/// always. The agent presents the set; the client picks one (or the ask is
/// cancelled). snake_case on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

/// One option the agent offers the client on a permission ask. The client
/// selects one (allow once / always, reject once / always) or the ask is
/// cancelled.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOption {
    pub option_id: String,
    pub name: String,
    pub kind: PermissionOptionKind,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

/// The selected-permission outcome: the option id the client picked. Carried
/// inside the RequestPermissionOutcome Selected variant; camelCase fields so
/// optionId matches the wire shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectedPermissionOutcome {
    pub option_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

/// The three-state outcome of a permission ask. Cancelled is the third
/// state (no payload): the run was cancelled (a session/cancel reaped the
/// pending ask, per the control-lease spec) rather than answered
/// allow/reject. Tagged by an inner outcome field, so the response nests two
/// outcome levels: the outer ResponsePermissionResponse field name, the
/// inner enum discriminator.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RequestPermissionOutcome {
    /// The ask was cancelled (the pending reverse request was reaped, not
    /// answered). No payload.
    Cancelled,
    /// The client selected one of the offered options.
    Selected(SelectedPermissionOutcome),
}

/// A reverse permission request the adapter sends to the client mid-turn: a
/// tool needs a human verdict before the run proceeds. The tool_call carries
/// the call under review (a ToolCallUpdate, not a full ToolCall); the options
/// are the verdicts the client may return. req_id closure on cancel is
/// enforced at the adapter: a session/cancel resolves this ask to Cancelled
/// rather than leaving a pending JSON-RPC id dangling.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestPermissionRequest {
    pub session_id: String,
    pub tool_call: crate::frontend::session_update::ToolCallUpdate,
    #[serde(default)]
    pub options: Vec<PermissionOption>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

/// The response to a reverse permission request. The outcome nests one level
/// (the outer field is outcome, the inner enum is also tagged by outcome),
/// matching the base protocol's double-outcome shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestPermissionResponse {
    pub outcome: RequestPermissionOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}
