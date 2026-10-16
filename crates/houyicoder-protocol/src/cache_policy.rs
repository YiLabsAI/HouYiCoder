//! Prompt-cache request types. A cache policy marks where the provider should
//! carve a stable prefix for prompt-cache reuse, with an ephemeral TTL hint;
//! the endpoint's declared convention says how that position is expressed.
//! The types are serde-only, so they ride the CompletionRequest without the
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

/// The cache hint attached to a breakpoint: an ephemeral entry with a TTL —
/// the provider reuses the prefix within the TTL window. A persistent tier
/// is deliberately absent: no current endpoint needs it, and it returns with
/// the capability catalog when a stateful one does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind", content = "ttl")]
pub enum CacheHint {
    Ephemeral(CacheTtl),
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

/// How an endpoint wants a cache position expressed. A breakpoint says where
/// the prefix should end; this says how the endpoint accepts that position.
/// The two axes are independent: a breakpoint whose position the declared
/// form cannot express is dropped rather than approximated.
///
/// - AutoPrefix: the endpoint caches the leading prefix by itself, so the
///   request carries no marker and a stable label names the prefix to reuse.
///   Undeclared is this form, the one the plain chat/completions endpoint
///   family expects.
/// - PositionalMarkers: the endpoint reuses only what a marker covers, so
///   each breakpoint is written at its own position and the label means
///   nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PromptCacheFormat {
    #[default]
    AutoPrefix,
    PositionalMarkers,
}

/// One cache breakpoint the policy placed on the request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheBreakpoint {
    pub kind: BreakpointKind,
    pub hint: CacheHint,
}

#[cfg(test)]
mod tests;
