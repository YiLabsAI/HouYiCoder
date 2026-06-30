//! Wire-level error contract — the serializable error mapped at the service
//! / transport boundary. Internal errors (per-crate, thiserror-style) never
//! cross the wire directly; the service maps them to a WireError at the
//! boundary. The kind set is derived from the real service, framing, and
//! transport failure surface, not copied from the tool-behavior error enum
//! (tool errors are tool-result content, not top-level wire errors).

use serde::{Deserialize, Serialize};

/// A closed set of wire-error kinds. Each names a distinct failure class the
/// frontend can branch on (retry vs surface vs re-auth). Variants are added
/// only when a real boundary failure demands a new branch, never
/// speculatively, so the set stays an honest contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WireErrorKind {
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

/// The wire error. Mapped at the service boundary from internal error types;
/// internal errors never serialize directly. retriable tells the frontend
/// whether to retry; correlation ties the error to the request or event it
/// failed for.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WireError {
    pub kind: WireErrorKind,
    pub message: String,
    pub retriable: bool,
    pub correlation: Option<String>,
}

impl WireError {
    pub fn new(kind: WireErrorKind, message: impl Into<String>, retriable: bool) -> Self {
        Self {
            kind,
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

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for WireError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wire_error_round_trips() {
        let e = WireError::new(WireErrorKind::Unavailable, "worker paused", true)
            .with_correlation("req-7");
        let json = serde_json::to_string(&e).expect("serialize");
        let back: WireError = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.kind, WireErrorKind::Unavailable);
        assert_eq!(back.message, "worker paused");
        assert!(back.retriable);
        assert_eq!(back.correlation.as_deref(), Some("req-7"));
    }

    #[test]
    fn test_kind_serializes_snake_case() {
        let e = WireError::new(WireErrorKind::ProtocolVersion, "x", false);
        let json = serde_json::to_string(&e).expect("serialize");
        assert!(
            json.contains("protocol_version"),
            "kind serializes snake_case for wire stability: {json}"
        );
    }
}
