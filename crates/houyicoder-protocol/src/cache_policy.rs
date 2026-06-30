//! Prompt-cache breakpoint wire types. A cache policy marks where the
//! provider should carve a stable prefix for prompt-cache reuse, and the
//! hint kind (ephemeral with a TTL, or persistent). The types are wire-only
//! (serde, no behavior) so they ride the CompletionRequest without the
//! protocol crate depending on a behavior crate. The policy trait that
//! produces these lives in the behavior crate.

use serde::{Deserialize, Serialize};

/// The cache TTL the provider should honor for an ephemeral breakpoint. The
/// one-hour floor (ttlSeconds >= 3600 maps to a 1h ephemeral); shorter values are provider-specific and not
/// modeled here yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CacheTtl {
    /// One-hour ephemeral cache (3600s floor).
    OneHour,
}

/// The cache hint attached to a breakpoint: an ephemeral entry with a TTL
/// (the common case — the provider reuses the prefix within the TTL window)
/// or a persistent entry (the provider keeps it across the session).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind", content = "ttl")]
pub enum CacheHint {
    Ephemeral(CacheTtl),
    Persistent,
}

/// Where a breakpoint sits. The kind is symbolic so the policy does not need
/// to know byte offsets — the provider lowers each kind to a concrete position
/// in its own wire format at request time.
///
/// - SystemStaticPrefix: the end of the byte-stable system prefix (the
///   dynamic_boundary offset the SystemPrompt records). Caching the static
///   prefix is the highest-value reuse — it is identical across every turn
///   of the session.
/// - LastToolDef: after the last tool definition. Tool schemas are large and
///   stable within a turn batch; caching them avoids re-billing 3-8k/turn.
/// - LatestUserMessage: the latest user message. The sliding dual-marker
///   strategy reads the prior turn's snapshot here; Auto
///   policy pins this position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BreakpointKind {
    SystemStaticPrefix,
    LastToolDef,
    LatestUserMessage,
}

/// One cache breakpoint the policy placed on the request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheBreakpoint {
    pub kind: BreakpointKind,
    pub hint: CacheHint,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_breakpoint_round_trips() {
        let bp = CacheBreakpoint {
            kind: BreakpointKind::SystemStaticPrefix,
            hint: CacheHint::Ephemeral(CacheTtl::OneHour),
        };
        let json = serde_json::to_string(&bp).expect("serialize");
        assert!(
            json.contains("systemStaticPrefix"),
            "camelCase kind: {json}"
        );
        assert!(json.contains("ephemeral"), "kind tag: {json}");
        assert!(json.contains("oneHour"), "ttl: {json}");
        let back: CacheBreakpoint = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, bp);
    }

    #[test]
    fn test_persistent_hint_round_trips() {
        let bp = CacheBreakpoint {
            kind: BreakpointKind::LastToolDef,
            hint: CacheHint::Persistent,
        };
        let json = serde_json::to_string(&bp).expect("serialize");
        assert!(json.contains("persistent"), "persistent tag: {json}");
        let back: CacheBreakpoint = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, bp);
    }
}
