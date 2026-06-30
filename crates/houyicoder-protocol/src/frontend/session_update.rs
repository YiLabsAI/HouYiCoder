//! ACP session/update wire type — the turn-level event stream the service
//! pushes to the client. A serde wire type matching the official
//! agent-client-protocol SessionUpdate shape (internally tagged by
//! sessionUpdate, camelCase fields, snake_case enum variants) so the
//! cross-decode fixture in service/tests/ gates fidelity against that
//! crate. The protocol layer owns these wire types; the engine TurnEvent
//! projects to them at the service boundary, so the frontend renders the full
//! turn stream without importing engine types.
//!
//! Only the standard variants the current server emits are typed here
//! (UserMessageChunk, AgentMessageChunk, AgentThoughtChunk, ToolCall,
//! ToolCallUpdate). The remaining ACP variants (Plan, AvailableCommandsUpdate,
//! CurrentModeUpdate, ConfigOptionUpdate, SessionInfoUpdate) land when the
//! ACP adapter phase emits them; a standard client ignores an unknown
//! sessionUpdate variant.
//!
//! Optional fields the server does not emit (_meta, annotations,
//! uri, unstable message_id) are omitted: serde ignores unknown fields on
//! decode so a peer emission carrying them still parses, and we never
//! serialize them. Kinds the base protocol has no counterpart for
//! (CompactionBoundary, Summary, PermissionDecision, MetaUser) do NOT ride
//! session/update — they ride the acpx/context/* extension notifications
//! (crate::acpx). The two streams are orthogonal: session/update is the
//! ACP-standard turn stream; acpx/context/* is the durable-context audit
//! stream.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::run::ContentBlock;

/// One chunk of streaming content (ACP ContentChunk). The full type adds
/// an unstable message_id; omitted here, decode parity holds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentChunk {
    pub content: ContentBlock,
}

impl ContentChunk {
    pub fn new(content: ContentBlock) -> Self {
        Self { content }
    }
}

/// Unique identifier for a tool call within a session (ACP ToolCallId). A
/// transparent newtype over a string so the wire shape is an unadorned
/// string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolCallId(pub String);

impl ToolCallId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl From<String> for ToolCallId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for ToolCallId {
    fn from(s: &str) -> Self {
        Self(s.into())
    }
}

impl std::fmt::Display for ToolCallId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The category of a tool call (ACP ToolKind). snake_case on the wire; Other
/// is the default a server emits when the tool does not fit a listed
/// category. The wire field name is kind because ACP defines it on the
/// ToolCall struct — that is an ACP-owned field, not our discriminator tag,
/// so cross-decode fidelity requires keeping it. non_exhaustive so a new
/// category lands without reworking every match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolKind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    SwitchMode,
    #[default]
    #[serde(other)]
    Other,
}

/// Execution status of a tool call (ACP ToolCallStatus). snake_case on the
/// wire; Pending is the default before the call runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolCallStatus {
    #[default]
    Pending,
    InProgress,
    Completed,
    Failed,
}

/// A file location a tool call touches (ACP ToolCallLocation). camelCase on
/// the wire; the optional line number and _meta the server does not emit are
/// omitted.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallLocation {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

impl ToolCallLocation {
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            line: None,
        }
    }
}

/// Content produced by a tool call (ACP ToolCallContent). Internally tagged
/// by type. The Content variant wraps a standard content block; Diff and
/// Terminal land when the server emits them. non_exhaustive.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolCallContent {
    Content { content: ContentBlock },
}

/// A tool call the model requested (ACP ToolCall). camelCase fields. The
/// server emits the fields the engine ToolCall event carries (tool_call_id,
/// title, kind, status, raw_input); content, locations, and raw_output are
/// optional and default empty when the engine did not produce them.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub tool_call_id: ToolCallId,
    pub title: String,
    #[serde(default, skip_serializing_if = "ToolKind::is_other")]
    pub kind: ToolKind,
    #[serde(default, skip_serializing_if = "ToolCallStatus::is_pending")]
    pub status: ToolCallStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<ToolCallContent>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub locations: Vec<ToolCallLocation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_input: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_output: Option<Value>,
}

impl ToolCall {
    pub fn new(tool_call_id: impl Into<ToolCallId>, title: impl Into<String>) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            title: title.into(),
            kind: ToolKind::Other,
            status: ToolCallStatus::Pending,
            content: Vec::new(),
            locations: Vec::new(),
            raw_input: None,
            raw_output: None,
        }
    }

    pub fn raw_input(mut self, input: Value) -> Self {
        self.raw_input = Some(input);
        self
    }

    pub fn raw_output(mut self, output: Value) -> Self {
        self.raw_output = Some(output);
        self
    }

    pub fn status(mut self, status: ToolCallStatus) -> Self {
        self.status = status;
        self
    }
}

impl ToolKind {
    fn is_other(&self) -> bool {
        matches!(self, ToolKind::Other)
    }
}

impl ToolCallStatus {
    fn is_pending(&self) -> bool {
        matches!(self, ToolCallStatus::Pending)
    }
}

/// The optional fields a ToolCallUpdate carries (ACP ToolCallUpdateFields).
/// Flattened into the surrounding ToolCallUpdate so the wire shape is the
/// fields inlined, matching the wire flatten. All optional; only the
/// fields the server updates are set.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallUpdateFields {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ToolCallStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_input: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_output: Option<Value>,
}

impl ToolCallUpdateFields {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn status(mut self, status: ToolCallStatus) -> Self {
        self.status = Some(status);
        self
    }

    pub fn raw_output(mut self, output: Value) -> Self {
        self.raw_output = Some(output);
        self
    }
}

/// An update to a tool call's status or results (ACP ToolCallUpdate). The
/// fields are flattened in so the wire shape is tool_call_id plus the
/// updated fields inlined, matching the wire flatten.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallUpdate {
    pub tool_call_id: ToolCallId,
    #[serde(flatten)]
    pub fields: ToolCallUpdateFields,
}

impl ToolCallUpdate {
    pub fn new(tool_call_id: impl Into<ToolCallId>, fields: ToolCallUpdateFields) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            fields,
        }
    }
}

/// One ACP session/update notification, wire form. Internally tagged by
/// sessionUpdate with snake_case variant names, matching the
/// SessionUpdate enum so the cross-decode fixture gates fidelity. The five
/// variants here are the standard kinds the current server emits; a future
/// variant lands without reworking every match (non_exhaustive).
///
/// - UserMessageChunk: a chunk of the user's message.
/// - AgentMessageChunk: a chunk of the agent's reply.
/// - AgentThoughtChunk: a chunk of the agent's reasoning.
/// - ToolCall: a new tool call was initiated.
/// - ToolCallUpdate: a tool call's status or results updated.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "sessionUpdate", rename_all = "snake_case")]
#[non_exhaustive]
pub enum SessionUpdate {
    UserMessageChunk(ContentChunk),
    AgentMessageChunk(ContentChunk),
    AgentThoughtChunk(ContentChunk),
    ToolCall(ToolCall),
    ToolCallUpdate(ToolCallUpdate),
    /// A plan the agent posted (entries with priority + status).
    Plan(Plan),
    /// The set of slash commands the agent advertises (for palette UI).
    AvailableCommandsUpdate(AvailableCommandsUpdate),
    /// The active session mode changed (e.g. plan/default/acceptEdits).
    CurrentModeUpdate(CurrentModeUpdate),
    /// A session config option changed (e.g. model, sandbox).
    ConfigOptionUpdate(ConfigOptionUpdate),
    /// Session metadata (title, updated_at) changed. Uses MaybeUndefined so
    /// a null field clears the value, an absent field leaves it unchanged.
    SessionInfoUpdate(SessionInfoUpdate),
}

/// A plan entry: a content line, a priority, and a status.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanEntry {
    pub content: String,
    pub priority: PlanEntryPriority,
    pub status: PlanEntryStatus,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<crate::acp_wire::Meta>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanEntryPriority {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanEntryStatus {
    Pending,
    InProgress,
    Completed,
}

/// A plan the agent posts: a list of entries.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    #[serde(default)]
    pub entries: Vec<PlanEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<crate::acp_wire::Meta>,
}

/// An available slash command the agent advertises.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableCommand {
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<AvailableCommandInput>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<crate::acp_wire::Meta>,
}

/// Unstructured command input (a hint string).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableCommandInput {
    pub hint: String,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<crate::acp_wire::Meta>,
}

/// The set of available commands.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableCommandsUpdate {
    #[serde(default)]
    pub available_commands: Vec<AvailableCommand>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<crate::acp_wire::Meta>,
}

/// A mode change (the active session mode id).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentModeUpdate {
    pub current_mode_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<crate::acp_wire::Meta>,
}

/// A config-option change (opaque first-cut: the option id + value).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigOptionUpdate {
    #[serde(default)]
    pub config_options: Vec<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<crate::acp_wire::Meta>,
}

/// Session metadata change. title/updated_at use MaybeUndefined so null
/// clears and absent leaves unchanged.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfoUpdate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<crate::acp_wire::Meta>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_message_round_trips() {
        // Internally tagged by sessionUpdate; the content chunk nests a
        // type-tagged content block. The wire shape is the ACP session/update
        // shape the cross-decode fixture gates.
        let u = SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text {
            text: "hi".into(),
        }));
        let json = serde_json::to_string(&u).expect("serialize");
        assert_eq!(
            json,
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}"#
        );
        let back: SessionUpdate = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(back, SessionUpdate::AgentMessageChunk(_)));
    }

    #[test]
    fn test_tool_call_round_trips() {
        // camelCase fields; tool_call_id is a transparent string. The kind
        // and status defaults are skipped when they hold the default so
        // the wire shape matches an emission that omits them.
        let tc = SessionUpdate::ToolCall(
            ToolCall::new("toolu_1", "bash")
                .raw_input(serde_json::Value::String("ls".into()))
                .status(ToolCallStatus::InProgress),
        );
        let json = serde_json::to_string(&tc).expect("serialize");
        assert!(
            json.contains(r#""toolCallId":"toolu_1""#),
            "camelCase: {json}"
        );
        assert!(json.contains(r#""title":"bash""#), "title: {json}");
        assert!(json.contains(r#""status":"in_progress""#), "status: {json}");
        assert!(json.contains(r#""rawInput":"ls""#), "rawInput: {json}");
        assert!(!json.contains("kind"), "default kind omitted: {json}");
    }

    #[test]
    fn test_tool_call_update_flattens() {
        // The update fields are flattened in: the wire shape is tool_call_id
        // plus the updated fields inlined, not nested under a fields key.
        let upd = SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "toolu_1",
            ToolCallUpdateFields::new()
                .status(ToolCallStatus::Completed)
                .raw_output(serde_json::Value::String("ok".into())),
        ));
        let json = serde_json::to_string(&upd).expect("serialize");
        assert!(
            !json.contains(r#""fields""#),
            "fields flattened, not nested: {json}"
        );
        assert!(json.contains(r#""toolCallId":"toolu_1""#), "{json}");
        assert!(json.contains(r#""status":"completed""#), "{json}");
        assert!(json.contains(r#""rawOutput":"ok""#), "{json}");
    }

    #[test]
    fn test_ignores_unknown_fields() {
        // A peer emission carrying _meta / annotations the server does not
        // emit still parses: serde ignores unknown fields, so decode parity
        // holds for the subset we model.
        let json = r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi","annotations":{"priority":"high"},"_meta":{"x":1}}}"#;
        let back: SessionUpdate = serde_json::from_str(json).expect("decode peer shape");
        assert!(matches!(back, SessionUpdate::AgentMessageChunk(_)));
    }

    #[test]
    fn test_chunks_round_trip() {
        for (name, upd) in [
            (
                "user_message_chunk",
                SessionUpdate::UserMessageChunk(ContentChunk::new(ContentBlock::Text {
                    text: "u".into(),
                })),
            ),
            (
                "agent_thought_chunk",
                SessionUpdate::AgentThoughtChunk(ContentChunk::new(ContentBlock::Text {
                    text: "t".into(),
                })),
            ),
        ] {
            let json = serde_json::to_string(&upd).expect("serialize");
            assert!(json.contains(&format!("\"{name}\"")), "{json}");
            let back: SessionUpdate = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(json, serde_json::to_string(&back).unwrap());
        }
    }
}
