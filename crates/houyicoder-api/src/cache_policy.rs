//! Prompt-cache policy trait. Providers lower prompt-cache breakpoints
//! differently on the wire (native cache_control, a transport-neutral
//! cache-policy SDK, sliding dual markers). houyi abstracts a single trait so
//! a multi-provider runtime shares one policy seam: the policy decides WHERE
//! to carve a stable prefix (the wire kinds in the wire crate's cache_policy
//! module), the provider decides HOW to lower each kind to its own wire
//! format.
//!
//! The Auto strategy places three breakpoints — the byte-stable system
//! prefix (the dynamic_boundary the SystemPrompt records, the highest-value
//! reuse), the last tool definition (tool schemas are large and stable
//! within a turn batch), and the latest user message (the sliding reuse
//! point). All ephemeral 1h.

use std::sync::Arc;

use houyicoder_protocol::cache_policy::{BreakpointKind, CacheBreakpoint, CacheHint, CacheTtl};

/// The cache policy a provider applies to a request. Auto places the default
/// three breakpoints; None disables cache hints (the provider uses its own
/// defaults). An explicit object form lands when a config-driven policy wires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CachePolicy {
    Auto,
    None,
}

/// A provider of cache breakpoints. The trait is the seam between the
/// request-building loop (which knows the served view) and the transport
/// (which lowers breakpoints to a wire format). The default implementation
/// places the Auto three-breakpoint set; a provider that wants no cache
/// hints returns None.
pub trait CachePolicyProvider: Send + Sync {
    /// The policy this provider applies.
    fn policy(&self) -> CachePolicy;

    /// Push the breakpoints for the policy onto the given vec. The vec is
    /// cleared first so a re-applied policy does not stack duplicates.
    fn apply(&self, breakpoints: &mut Vec<CacheBreakpoint>, policy: &CachePolicy) {
        breakpoints.clear();
        if matches!(policy, CachePolicy::Auto) {
            // The three-breakpoint Auto set: system static prefix, last tool
            // definition, latest user message. All ephemeral 1h.
            let hint = CacheHint::Ephemeral(CacheTtl::OneHour);
            breakpoints.push(CacheBreakpoint {
                kind: BreakpointKind::SystemStaticPrefix,
                hint: hint.clone(),
            });
            breakpoints.push(CacheBreakpoint {
                kind: BreakpointKind::LastToolDef,
                hint: hint.clone(),
            });
            breakpoints.push(CacheBreakpoint {
                kind: BreakpointKind::LatestUserMessage,
                hint,
            });
        }
    }
}

/// The default Auto cache policy. Places the three-breakpoint set on every
/// request. Constructed once at the composition root + shared (Arc) so the
/// provider's apply is cheap.
#[derive(Debug, Default)]
pub struct AutoCachePolicy;

impl CachePolicyProvider for AutoCachePolicy {
    fn policy(&self) -> CachePolicy {
        CachePolicy::Auto
    }
}

/// A cache policy that disables cache hints. Used by tests + providers that
/// have no prompt-cache support (Gemini-style skip).
#[derive(Debug, Default)]
pub struct NoCachePolicy;

impl CachePolicyProvider for NoCachePolicy {
    fn policy(&self) -> CachePolicy {
        CachePolicy::None
    }
}

/// A type-erased cache policy handle the runner holds. Arc so the shared
/// runner (server + TUI) shares one policy instance.
pub type SharedCachePolicy = Arc<dyn CachePolicyProvider>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auto_places_three_breakpoints() {
        let policy = AutoCachePolicy;
        let mut breakpoints = Vec::new();
        policy.apply(&mut breakpoints, &policy.policy());
        assert_eq!(breakpoints.len(), 3, "Auto places 3 breakpoints");
        // Order: system static prefix, last tool def, latest user message.
        assert_eq!(breakpoints[0].kind, BreakpointKind::SystemStaticPrefix);
        assert_eq!(breakpoints[1].kind, BreakpointKind::LastToolDef);
        assert_eq!(breakpoints[2].kind, BreakpointKind::LatestUserMessage);
        // All ephemeral 1h.
        assert!(
            breakpoints
                .iter()
                .all(|bp| matches!(bp.hint, CacheHint::Ephemeral(CacheTtl::OneHour)))
        );
    }

    #[test]
    fn test_none_places_no_breakpoints() {
        let policy = NoCachePolicy;
        let mut breakpoints = vec![CacheBreakpoint {
            kind: BreakpointKind::SystemStaticPrefix,
            hint: CacheHint::Persistent,
        }];
        policy.apply(&mut breakpoints, &policy.policy());
        assert!(breakpoints.is_empty(), "None clears breakpoints");
    }

    #[test]
    fn test_apply_clears_before_pushing() {
        // Re-applying Auto does not stack duplicates (the vec clears first).
        let policy = AutoCachePolicy;
        let mut breakpoints = Vec::new();
        policy.apply(&mut breakpoints, &CachePolicy::Auto);
        policy.apply(&mut breakpoints, &CachePolicy::Auto);
        assert_eq!(breakpoints.len(), 3, "re-apply does not stack");
    }

    #[test]
    fn test_policy_reports_mode() {
        assert_eq!(AutoCachePolicy.policy(), CachePolicy::Auto);
        assert_eq!(NoCachePolicy.policy(), CachePolicy::None);
    }
}
