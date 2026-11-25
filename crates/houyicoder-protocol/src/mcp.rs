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
/// The standard config file spells a stdio server flat, with a command field
/// and no kind field, and spells an HTTP server with a URL. The parser accepts
/// every spelling: when the kind is absent, a body carrying a URL and no
/// command is HTTP and one carrying a command is stdio, so a copied file
/// cannot be mistaken for the other transport.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpTransportConfig {
    /// The transport kind.
    pub kind: McpTransportKind,
    /// The command to launch, or the endpoint URL to call.
    pub endpoint: String,
    /// Arguments after the command. Empty for HTTP.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<String>,
    /// Variables the child needs, already expanded. Empty for HTTP.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub environment: BTreeMap<String, String>,
    /// Headers an HTTP server requires, after expansion. Empty for stdio.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// A command whose stdout supplies HTTP headers at connect time. Absent
    /// for stdio and for servers with static headers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers_helper: Option<String>,
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
            headers_helper: None,
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
            headers_helper: None,
        }
    }
}

/// The body as a config file writes it, before the kind decides which of the
/// two address fields is the endpoint. Kept separate so normalization happens
/// in one place rather than through aliases that cannot see the other fields.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawTransport {
    #[serde(default, rename = "type", alias = "kind")]
    kind: Option<McpTransportKind>,
    /// The standard file spells the command under this key; the older
    /// environment spelling called it a program.
    #[serde(default, alias = "program")]
    command: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default, alias = "args")]
    arguments: Vec<String>,
    #[serde(default, alias = "env")]
    environment: BTreeMap<String, String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default, rename = "headersHelper")]
    headers_helper: Option<String>,
}

impl<'de> Deserialize<'de> for McpTransportConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawTransport::deserialize(deserializer)?;
        // A body with no kind is judged by which address it carries: a URL
        // alone is an HTTP server, a command is a child process.
        let kind = raw.kind.unwrap_or({
            if raw.url.is_some() && raw.command.is_none() {
                McpTransportKind::Http
            } else {
                McpTransportKind::Stdio
            }
        });
        let endpoint = match kind {
            McpTransportKind::Stdio => raw.command.or(raw.endpoint).or(raw.url),
            McpTransportKind::Http => raw.url.or(raw.endpoint).or(raw.command),
        }
        .unwrap_or_default();
        Ok(Self {
            kind,
            endpoint,
            arguments: raw.arguments,
            environment: raw.environment,
            headers: raw.headers,
            headers_helper: raw.headers_helper,
        })
    }
}

/// The transport kinds a server can be reached over.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpTransportKind {
    /// A child process speaking the protocol over its standard streams.
    #[default]
    Stdio,
    /// A streamable HTTP endpoint. A file may spell it as the older
    /// server-sent-events transport, which this kind carries.
    #[serde(alias = "streamable-http", alias = "sse")]
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

/// One tool as the /mcp detail page renders it: its name, whether its
/// annotations claim it safe to run without asking, and its description.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolSummary {
    pub name: String,
    pub read_only: bool,
    pub description: String,
}

/// The fields one server's detail page renders beyond its list row: the
/// transport endpoint, the header names it was configured with, the loaded
/// tools, a preview of its instructions, and whether an untrusted workspace
/// holds the server off. Header values never cross this boundary, so a
/// secret one configures cannot reach a view that renders it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerDetailView {
    /// The command the host launches, or the endpoint URL it calls.
    pub endpoint: String,
    /// The header names an HTTP server was configured with. The pane renders
    /// each as a masked name-value pair.
    pub header_names: Vec<String>,
    pub tools: Vec<McpToolSummary>,
    /// The first characters of the instructions the server exported.
    pub instructions_preview: Option<String>,
    /// True when the server is off because its workspace is not trusted, so
    /// enabling it is refused rather than merely undoable.
    pub untrusted_lock: bool,
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
    #[serde(default)]
    pub detail: McpServerDetailView,
}

/// The default tool call timeout: long enough for a real tool to work, short
/// enough that a wedged call cannot hold a turn.
fn default_tool_timeout_ms() -> u64 {
    60_000
}

#[cfg(test)]
mod tests;
