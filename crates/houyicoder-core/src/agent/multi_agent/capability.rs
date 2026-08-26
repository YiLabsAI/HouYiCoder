//! Capability token: the effective permission mode a child agent runs
//! with, computed as the intersection of the parent's mode and the
//! agent definition's declared mode.
//!
//! The invariant: child capability = parent ∩ declared. This is a
//! monotonic narrowing — a child can never hold a capability the
//! parent does not. An agent definition that asks for a mode the
//! parent lacks is downgraded to the parent's mode, never rejected
//! (the agent still runs, just with less autonomy than it asked for).

use houyicoder_protocol::frontend::permission::PermissionMode;

/// The effective permission mode a child runs with. Carried from
/// spawn time through the child Runner so every tool gate decision
/// uses the narrowed mode, not the parent's or the definition's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapabilityToken {
    mode: PermissionMode,
}

impl CapabilityToken {
    /// Compute the effective mode: if the declared mode is wider
    /// than the parent, the child gets the parent's (narrower).
    /// If the declared mode is the same or narrower, the child
    /// gets the declared mode. An unknown declared mode fails safe
    /// to the parent's mode.
    pub fn effective(parent: PermissionMode, declared: Option<PermissionMode>) -> Self {
        match declared {
            None => Self { mode: parent },
            Some(declared) => Self {
                mode: narrower(parent, declared),
            },
        }
    }

    pub fn mode(&self) -> PermissionMode {
        self.mode
    }
}

/// Return the narrower of two permission modes. Manual is narrower
/// than Auto (Auto allows everything Manual allows, plus destructive
/// ops without asking). A child can never hold a wider mode than its
/// parent. Unknown modes (future enum variants) fail safe to Manual.
fn narrower(a: PermissionMode, b: PermissionMode) -> PermissionMode {
    match (a, b) {
        (PermissionMode::Manual, _) | (_, PermissionMode::Manual) => PermissionMode::Manual,
        (PermissionMode::Auto, PermissionMode::Auto) => PermissionMode::Auto,
        // Future modes: fail safe to the narrowest known mode.
        _ => PermissionMode::Manual,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inherits_parent_undeclared() {
        let t = CapabilityToken::effective(PermissionMode::Auto, None);
        assert_eq!(t.mode(), PermissionMode::Auto);
        let t = CapabilityToken::effective(PermissionMode::Manual, None);
        assert_eq!(t.mode(), PermissionMode::Manual);
    }

    #[test]
    fn test_narrowed_parent_manual() {
        let t = CapabilityToken::effective(PermissionMode::Manual, Some(PermissionMode::Auto));
        assert_eq!(
            t.mode(),
            PermissionMode::Manual,
            "parent Manual narrows child"
        );
    }

    #[test]
    fn test_auto_parent_auto_child() {
        let t = CapabilityToken::effective(PermissionMode::Auto, Some(PermissionMode::Auto));
        assert_eq!(t.mode(), PermissionMode::Auto);
    }

    #[test]
    fn test_manual_both() {
        let t = CapabilityToken::effective(PermissionMode::Manual, Some(PermissionMode::Manual));
        assert_eq!(t.mode(), PermissionMode::Manual);
    }

    /// An agent definition that asks for a mode the parent does not
    /// have is downgraded, not amplified. The child runs with the
    /// parent's narrower mode, not the agent's wider one.
    #[test]
    fn test_never_amplifies() {
        let t = CapabilityToken::effective(PermissionMode::Manual, Some(PermissionMode::Auto));
        assert_eq!(t.mode(), PermissionMode::Manual);
    }

    /// Monotonic: if the parent is already the narrowest, the child
    /// cannot be wider regardless of what the agent declares.
    #[test]
    fn test_monotonic_narrowing() {
        for declared in [PermissionMode::Manual, PermissionMode::Auto] {
            let t = CapabilityToken::effective(PermissionMode::Manual, Some(declared));
            assert_eq!(
                t.mode(),
                PermissionMode::Manual,
                "parent Manual, declared {declared:?}: child must be Manual"
            );
        }
    }
}
