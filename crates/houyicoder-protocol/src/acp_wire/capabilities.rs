//! Capability blocks advertised on the initialize handshake. The client
//! describes what it supports (filesystem, terminal); the agent describes
//! what it supports (load_session, prompt, mcp, session). Gated capability
//! kinds are unknown-field-dropped on decode so a stock peer that carries
//! extra fields does not fail the handshake.

use serde::{Deserialize, Serialize};

use super::Meta;

/// Identifies an implementation (client or agent): name + version + optional
/// title. The adapter fills agent_info with its own identity on the
/// initialize response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Implementation {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

/// The filesystem capabilities a stock client advertises. Both bools default
/// false; the adapter reads them to know whether it may use the
/// fs/read_text_file and fs/write_text_file reverse verbs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSystemCapabilities {
    #[serde(default)]
    pub read_text_file: bool,
    #[serde(default)]
    pub write_text_file: bool,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

/// The capabilities a stock client advertises on initialize. The first-cut
/// mirror covers the stable (non-gated) fields: filesystem + terminal. Gated
/// capability kinds (auth, elicitation, nes, position encodings) are unknown
/// fields the client may carry; serde drops them on decode rather than
/// failing the handshake.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    #[serde(default)]
    pub fs: FileSystemCapabilities,
    #[serde(default)]
    pub terminal: bool,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

/// The agent's prompt capabilities (image, audio, embedded context). All
/// default false; the adapter sets the ones its model actually accepts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptCapabilities {
    #[serde(default)]
    pub image: bool,
    #[serde(default)]
    pub audio: bool,
    #[serde(default)]
    pub embedded_context: bool,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

/// The agent's MCP capabilities (http, sse). All default false.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpCapabilities {
    #[serde(default)]
    pub http: bool,
    #[serde(default)]
    pub sse: bool,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

/// The agent's session capabilities. The first-cut mirror carries the stable
/// list marker; the gated fork/resume/close/additional_directories fields
/// are unknown-field-dropped on decode until the adapter opts into the
/// unstable surface.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list: Option<SessionListCapabilities>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

/// The empty list-sessions capability marker (present = supported).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionListCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

/// The capabilities the agent advertises on the initialize response. The
/// first-cut mirror covers the stable fields (load_session, prompt, mcp,
/// session). Gated kinds (auth, nes, position_encoding) are
/// unknown-field-dropped on decode.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapabilities {
    #[serde(default)]
    pub load_session: bool,
    #[serde(default)]
    pub prompt_capabilities: PromptCapabilities,
    #[serde(default)]
    pub mcp_capabilities: McpCapabilities,
    #[serde(default)]
    pub session_capabilities: SessionCapabilities,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}
