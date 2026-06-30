//! Session lifecycle: create a fresh session or reconnect to an existing one.
//! The client pins a cwd plus MCP servers; the adapter spawns or reattaches
//! the engine run. A session-indexed pending permission is re-sent on
//! load (the control-lease spec).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::Meta;

/// Create a session: the client pins a cwd + MCP servers. The adapter spawns
/// the engine run for a fresh session. mcp_servers is opaque JSON in the
/// first cut (the typed McpServer shape lands when the adapter wires MCP).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionRequest {
    pub cwd: String,
    #[serde(default)]
    pub mcp_servers: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

/// The new-session response: the session id the adapter minted. modes/models
/// and config_options are Option and omitted from the first cut.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionResponse {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

/// Reconnect to an existing session: the client supplies the id + a cwd + MCP
/// servers. The adapter reattaches; a session-indexed pending permission is
/// re-sent (the control-lease spec). mcp_servers is opaque in the first cut.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadSessionRequest {
    pub session_id: String,
    pub cwd: String,
    #[serde(default)]
    pub mcp_servers: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

/// The load-session response: modes/models/config_options are Option and
/// omitted from the first cut; the reattach is acknowledged by the meta-only
/// shape.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadSessionResponse {
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}
