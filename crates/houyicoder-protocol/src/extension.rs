//! extension — tool / resource / hook / skill / WASM-plugin ABI.
//!
//! The wire forms (ToolCall, ToolResult, ToolError) that cross the model and
//! (in mode B) the transport boundary live here. The Tool behavior trait
//! lives in the ports crate and references these.

use serde::{Deserialize, Serialize};

/// A tool execution error. Becomes tool-result content — the model sees the
/// error string and can react; the loop does not abort. Only covers what a
/// Tool returns from execute: parameter and execution failures. Registry
/// misses, user rejections, and run interruptions are not tool errors (they
/// are dispatcher / control-flow outcomes) and live elsewhere.
///
/// Every variant carries the verbatim message the model saw before the
/// reshape, so the Display output stays bit-equivalent. The kind tag gives
/// the wire a structured signal the bare-string form lacked.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "message")]
pub enum ToolError {
    /// The model passed a missing or malformed argument
    /// (e.g. a required string field absent).
    InvalidInput(String),
    /// A path the tool was given escapes the workspace confinement
    /// (the sandbox hard-deny, zero-config fail-closed).
    PathEscapes(String),
    /// A generic execution failure wrapping an underlying error
    /// (e.g. a command exit, a filesystem op other than access/decode).
    Failed(String),
    /// Content the tool read could not be decoded
    /// (e.g. a binary file where utf-8 was required).
    Decode(String),
    /// A filesystem access failure
    /// (e.g. a path or workspace root not reachable).
    Io(String),
}

impl ToolError {
    /// The verbatim message the model sees (after the tool-error prefix).
    pub fn message(&self) -> &str {
        match self {
            Self::InvalidInput(m)
            | Self::PathEscapes(m)
            | Self::Failed(m)
            | Self::Decode(m)
            | Self::Io(m) => m,
        }
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "tool error: {}", self.message())
    }
}

impl std::error::Error for ToolError {}
