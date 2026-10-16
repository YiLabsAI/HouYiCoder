//! Protocol error contract — the serializable error mapped at the service /
//! transport boundary. Internal errors (per-crate, thiserror-style) never
//! cross the boundary directly; the service maps them to a ProtocolError.
//! The category set is derived from the real service, framing, and transport
//! failure surface, not copied from the tool-behavior error enum (tool
//! errors are tool-result content, not top-level protocol errors).

use crate::framing::FrameError;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fmt::Display;
use std::fmt::Formatter;

/// A closed set of protocol error categories. Each names a distinct failure
/// class the frontend can branch on (retry vs surface vs re-auth). Variants
/// are added only when a real boundary failure demands a new branch, never
/// speculatively, so the set stays an honest contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ErrorCategory {
    /// The peer speaks an incompatible protocol version (Hello mismatch).
    ProtocolVersion,
    /// A frame could not be parsed (bad framing, truncated, malformed JSON).
    InvalidFrame,
    /// A single message exceeded the protocol's per-message byte bound. Distinct
    /// from InvalidFrame: the message is well-formed, and the fault is its
    /// size, which a retry of the same bytes cannot fix.
    MessageTooLarge,
    /// A well-formed frame that is not a valid request for the current state.
    InvalidRequest,
    /// The caller lacks the capability or credential for the request.
    Unauthorized,
    /// The service is not currently reachable or is overloaded.
    Unavailable,
    /// An internal failure with no more specific class; retriable carries the
    /// recovery hint.
    Internal,
}

/// The protocol error. Mapped at the service boundary from internal error
/// types; internal errors never serialize directly. retriable tells the
/// frontend whether to retry; correlation ties the error to the request or
/// event it failed for. Display carries the user-readable message only — the
/// category is a structured field for branching, never display text.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProtocolError {
    pub category: ErrorCategory,
    pub message: String,
    pub retriable: bool,
    pub correlation: Option<String>,
}

impl ProtocolError {
    pub fn new(category: ErrorCategory, message: impl Into<String>, retriable: bool) -> Self {
        Self {
            category,
            message: message.into(),
            retriable,
            correlation: None,
        }
    }

    pub fn with_correlation(mut self, correlation: impl Into<String>) -> Self {
        self.correlation = Some(correlation.into());
        self
    }
}

impl From<FrameError> for ProtocolError {
    /// Framing failures cross the boundary as the single conversion point;
    /// producers rely on this instead of mapping the error at each call site.
    /// A frame over the per-message bound keeps its own category so a consumer
    /// can report a size refusal instead of a malformed frame, and neither is
    /// retriable: the same bytes would fail the same way.
    fn from(e: FrameError) -> Self {
        let category = match e {
            FrameError::TooLarge { .. } => ErrorCategory::MessageTooLarge,
            FrameError::Json(_) => ErrorCategory::InvalidFrame,
        };
        Self::new(category, e.to_string(), false)
    }
}

impl Display for ProtocolError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for ProtocolError {}

#[cfg(test)]
mod tests;
