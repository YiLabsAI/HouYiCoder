//! MCP configuration and status types shared by every layer that names an
//! external tool server: the config reader that resolves scopes, the
//! connection manager that reports state, and the surfaces that render it.
//! Pure data with no transport and no runtime, so the protocol leaf stays
//! free of the client stack.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Where one server's configuration came from, which decides both its
/// precedence when two scopes name the same server and whether starting a
/// process from it needs the workspace to be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpConfigScope {
    /// The user's own settings file. Highest precedence.
    User,
    /// The workspace's mcp.json file. Requires workspace trust to spawn.
    Project,
    /// The environment variable a caller passes servers through.
    Env,
}

/// How one server is reached: a child process over standard streams, or a
/// streamable HTTP endpoint.
///
/// The standard config file spells stdio servers flat, with a command field
/// and no kind field, and spells HTTP servers with a type field. The struct
/// accepts both: serde aliases each source field onto the one destination,
/// so the file a user copied from another client parses without a rewrite.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpTransportConfig {
    /// The transport kind. Omitted in the common stdio spelling of the
    /// standard config file, so the field defaults to stdio; the typed
    /// spelling names it under the file's own key.
    #[serde(default, alias = "type")]
    pub kind: McpTransportKind,
    /// The command to launch, or the endpoint URL to call.
    #[serde(alias = "command", alias = "url")]
    pub endpoint: String,
    /// Arguments after the command. Empty for HTTP.
    #[serde(default, alias = "args")]
    pub arguments: Vec<String>,
    /// Variables the child needs, already expanded. Empty for HTTP.
    #[serde(default, alias = "env")]
    pub environment: BTreeMap<String, String>,
    /// Headers an HTTP server requires, after expansion. Empty for stdio.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

impl McpTransportConfig {
    /// The stdio transport over one command.
    pub fn stdio(command: impl Into<String>, arguments: Vec<String>) -> Self {
        Self {
            kind: McpTransportKind::Stdio,
            endpoint: command.into(),
            arguments,
            environment: BTreeMap::new(),
            headers: BTreeMap::new(),
        }
    }

    /// The streamable HTTP transport over one endpoint.
    pub fn http(url: impl Into<String>) -> Self {
        Self {
            kind: McpTransportKind::Http,
            endpoint: url.into(),
            arguments: Vec::new(),
            environment: BTreeMap::new(),
            headers: BTreeMap::new(),
        }
    }
}

/// The transport kinds a server can be reached over.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpTransportKind {
    /// A child process speaking the protocol over its standard streams.
    /// Aliases the type spelling the standard config file uses.
    #[default]
    #[serde(alias = "type", alias = "stdio")]
    Stdio,
    /// A streamable HTTP endpoint.
    #[serde(alias = "http", alias = "streamable-http", alias = "sse")]
    Http,
}

/// One resolved server entry: a name, the scope it came from, and its
/// transport. The name is the key every later layer uses, so two scopes
/// naming one server resolve to one entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScopedMcpServerConfig {
    pub name: String,
    pub scope: McpConfigScope,
    pub transport: McpTransportConfig,
    #[serde(default)]
    pub disabled: bool,
    /// How long one tool call on this server may run before it is cancelled.
    #[serde(default = "default_tool_timeout_ms")]
    pub tool_timeout_ms: u64,
}

/// The behavior hints the MCP specification lets a server attach to a tool.
/// Absent fields are unknown, never false: a server that says nothing about
/// safety is treated as unsafe.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct McpToolAnnotations {
    pub read_only_hint: Option<bool>,
    pub destructive_hint: Option<bool>,
    pub idempotent_hint: Option<bool>,
    pub open_world_hint: Option<bool>,
}

/// The connection state of one managed server, as the surfaces render it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum McpConnectionState {
    Connected,
    Pending,
    Reconnecting { attempt: u32 },
    NeedsAuth { reason: String },
    Failed { error: String },
    Disabled { reason: String },
}

/// A read-only snapshot of one server, shared by the status surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerStatusView {
    pub name: String,
    pub scope: McpConfigScope,
    pub state: McpConnectionState,
    pub server_version: Option<String>,
    pub tool_count: usize,
    pub resource_count: usize,
    pub has_instructions: bool,
    pub recent_stderr: Vec<String>,
}

/// The default tool call timeout: long enough for a real tool to work, short
/// enough that a wedged call cannot hold a turn.
fn default_tool_timeout_ms() -> u64 {
    60_000
}

#[cfg(test)]
mod tests;
