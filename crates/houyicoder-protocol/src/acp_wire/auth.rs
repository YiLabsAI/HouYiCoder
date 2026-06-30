//! Authentication method shapes the agent advertises on the initialize
//! response. The default variant is the agent method (untagged on the wire);
//! gated variants are unknown-field-dropped on decode.

use serde::{Deserialize, Serialize};

use super::Meta;

/// An authentication method the agent advertises. The default variant is the
/// agent method (untagged on the wire — no type discriminator field); the
/// env-var and terminal variants carry a type tag and are gated. The
/// first-cut mirror models the default agent method shape; the gated
/// variants are unknown-field-dropped.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthMethod {
    /// The default agent method (untagged when no type field is present).
    #[serde(untagged)]
    Agent(AuthMethodAgent),
}

/// An agent-issued auth method: an id + name + optional description.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthMethodAgent {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}
