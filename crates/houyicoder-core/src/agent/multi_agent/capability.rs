//! Capability token: the effective permission mode a child agent runs
//! with, computed as parent ∩ declared (monotonic narrowing — a child
//! never holds a capability the parent does not; an over-asking
//! definition is downgraded, not rejected).
//!
//! Status: the narrowing logic is implemented + unit-tested but not yet
//! wired into the spawn path — neither spawn path calls effective, so
//! the property is not enforced end-to-end. Defer to a wiring task.

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
mod tests;
