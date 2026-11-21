//! Session lifecycle: create a fresh session or reconnect to an existing one.
//! The client pins a cwd plus MCP servers; the adapter spawns or reattaches
//! the engine run. A session-indexed pending permission is re-sent on
//! load (the control-lease spec).

use serde::{Deserialize, Serialize};

use super::Meta;

/// One header an HTTP or SSE MCP server is reached with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpHeader {
    pub name: String,
    pub value: String,
}

/// One environment variable an MCP server process is launched with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvVariable {
    pub name: String,
    pub value: String,
}

/// An HTTP-transport MCP server the client asks the agent to connect to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerHttp {
    pub name: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<HttpHeader>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

/// An SSE-transport MCP server the client asks the agent to connect to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerSse {
    pub name: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<HttpHeader>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

/// A stdio-transport MCP server the client asks the agent to launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerStdio {
    pub name: String,
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<EnvVariable>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

/// An MCP server entry the client names at session creation. Http and Sse
/// carry an explicit type tag; a Stdio entry may arrive without one, so its
/// shape matches by fields alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpServer {
    Http(McpServerHttp),
    Sse(McpServerSse),
    #[serde(untagged)]
    Stdio(McpServerStdio),
}

/// Create a session: the client pins the working directory, extra workspace
/// roots, and the MCP servers to connect. The adapter binds or spawns the
/// engine run; a binding composition root answers with the id the runner
/// actually drives.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionRequest {
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub additional_directories: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<McpServer>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

/// A mode the agent can operate in, such as a permission strictness level.
/// The id is what later set-mode requests name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMode {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

/// The mode state a session reports: which mode is current and the full set
/// the agent offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionModeState {
    pub current_mode_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_modes: Vec<SessionMode>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

/// One selectable value of a config option.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfigSelectOption {
    pub value: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

/// The type-specific payload of a config option. Select carries a current
/// value and the options to switch to; Boolean carries a current on/off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionConfigKind {
    Select {
        #[serde(rename = "currentValue")]
        current_value: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        options: Vec<SessionConfigSelectOption>,
    },
    Boolean {
        #[serde(rename = "currentValue")]
        current_value: bool,
    },
}

/// A config option the session offers, such as which model it runs. The
/// semantic category is advisory for the client's UI, not for correctness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfigOption {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(flatten)]
    pub kind: SessionConfigKind,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

/// The new-session response: the session id the runner is bound to, plus the
/// mode state and config options the session offers.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionResponse {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modes: Option<SessionModeState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_options: Option<Vec<SessionConfigOption>>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

/// Request parameters for session/set_mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetSessionModeRequest {
    pub session_id: String,
    pub mode_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

/// Response to session/set_mode. The body is empty: a mode change reaches the
/// client as a session/update current_mode_update notification, not in the
/// reply. Mirrors the official schema 1.5 SetSessionModeResponse.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetSessionModeResponse {
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

/// The value a set-config-option request carries. A select option names a
/// value id; a boolean option carries true or false.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionConfigOptionValue {
    #[serde(rename = "boolean")]
    Boolean { value: bool },
    #[serde(untagged)]
    ValueId { value: String },
}

/// Request parameters for session/set_config_option. Switching the model is
/// the option whose id the agent defined with category model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetSessionConfigOptionRequest {
    pub session_id: String,
    pub config_id: String,
    #[serde(flatten)]
    pub value: SessionConfigOptionValue,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

/// Response to session/set_config_option. The body carries the full config
/// options set after the switch, so the client sees the new current value.
/// Mirrors the official schema 1.5 SetSessionConfigOptionResponse.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetSessionConfigOptionResponse {
    pub config_options: Vec<SessionConfigOption>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

/// Reconnect to an existing session: the client supplies the id + a cwd + MCP
/// servers. The adapter reattaches; a session-indexed pending permission is
/// re-sent (the control-lease spec).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadSessionRequest {
    pub session_id: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub additional_directories: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<McpServer>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

/// The load-session response: the reattach is acknowledged by the meta-only
/// shape; a full implementation may carry mode state and config options.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadSessionResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modes: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_options: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}
