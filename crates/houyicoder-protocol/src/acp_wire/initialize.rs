//! The initialize handshake: the client's request and the agent's response.
//! The response carries the agent's capabilities and the extension
//! capability block at the _meta key so a reconnecting client knows lease
//! and detach support before it loads a pending ask.

use serde::{Deserialize, Serialize};

use super::{
    AgentCapabilities, AuthMethod, ClientCapabilities, Implementation, Meta, ProtocolVersion,
};

/// The initialize request a client sends on connect. The adapter replies with
/// an InitializeResponse carrying its own capabilities and the acpx
/// capability block at _meta.acpx.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeRequest {
    pub protocol_version: ProtocolVersion,
    #[serde(default)]
    pub client_capabilities: ClientCapabilities,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_info: Option<Implementation>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

/// The initialize response the adapter sends back. agent_capabilities
/// advertises what the agent supports; _meta.acpx carries the typed
/// AcpxCapabilities block (streaming/cas/detach + ext_method verbs) so a
/// reconnecting client knows lease/detach support before loadSession resends
/// a pending ask.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResponse {
    pub protocol_version: ProtocolVersion,
    #[serde(default)]
    pub agent_capabilities: AgentCapabilities,
    #[serde(default)]
    pub auth_methods: Vec<AuthMethod>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_info: Option<Implementation>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}
