//! Protocol error contract — the serializable error mapped at the service /
//! transport boundary. Internal errors (per-crate, thiserror-style) never
//! cross the boundary directly; the service maps them to a ProtocolError.
//! The category set is derived from the real service, framing, and transport
//! failure surface, not copied from the tool-behavior error enum (tool
//! errors are tool-result content, not top-level protocol errors).

use crate::framing::FrameError;
use serde::{Deserialize, Serialize};

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
    /// producers rely on this instead of hand-rolled mapping helpers.
    fn from(e: FrameError) -> Self {
        Self::new(ErrorCategory::InvalidFrame, e.to_string(), false)
    }
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ProtocolError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_json_byte_exact() {
        let e = ProtocolError::new(ErrorCategory::Internal, "failed", false);
        assert_eq!(
            serde_json::to_string(&e).expect("serialize"),
            r#"{"category":"internal","message":"failed","retriable":false,"correlation":null}"#
        );
    }

    #[test]
    fn test_error_round_trips() {
        let e = ProtocolError::new(ErrorCategory::Unavailable, "worker paused", true)
            .with_correlation("req-7");
        let json = serde_json::to_string(&e).expect("serialize");
        let back: ProtocolError = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.category, ErrorCategory::Unavailable);
        assert_eq!(back.message, "worker paused");
        assert!(back.retriable);
        assert_eq!(back.correlation.as_deref(), Some("req-7"));
    }

    #[test]
    fn test_category_serializes_snake_case() {
        let e = ProtocolError::new(ErrorCategory::ProtocolVersion, "x", false);
        let json = serde_json::to_string(&e).expect("serialize");
        assert!(
            json.contains("protocol_version"),
            "category serializes snake_case for protocol stability: {json}"
        );
    }

    #[test]
    fn test_display_is_message_only() {
        let e = ProtocolError::new(ErrorCategory::Internal, "failed to save settings", false);
        assert_eq!(e.to_string(), "failed to save settings");
    }

    #[test]
    fn test_frame_error_converts() {
        let frame_err = FrameError::from(serde_json::from_str::<u8>("{").unwrap_err());
        let e = ProtocolError::from(frame_err);
        assert_eq!(e.category, ErrorCategory::InvalidFrame);
        assert!(!e.retriable);
        assert!(!e.message.is_empty(), "carries the frame failure detail");
    }
}
