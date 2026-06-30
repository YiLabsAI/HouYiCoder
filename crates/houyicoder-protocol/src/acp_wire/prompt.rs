//! The prompt request and response: the client drives one turn with a list
//! of content blocks; the adapter forwards them to the engine run and
//! streams session/update back. The response carries the stop reason.

use serde::{Deserialize, Serialize};

use super::Meta;

/// A prompt request: the client drives one turn with a list of content
/// blocks. The adapter forwards the text blocks to the engine run and
/// streams session/update back. message_id is gated (unstable_message_id)
/// and omitted from the first-cut mirror; a client carrying it is
/// unknown-field-dropped.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptRequest {
    pub session_id: String,
    pub prompt: Vec<crate::frontend::run::ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

/// A prompt response: the turn ended with a stop reason. usage is gated
/// (unstable_session_usage) and omitted from the first-cut mirror.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptResponse {
    pub stop_reason: crate::frontend::run::StopReason,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}
