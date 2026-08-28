//! Trust state of a configuration source. Lives in the port crate so the
//! harness, the hook subsystem, and the skill trust-gate share one type
//! instead of each re-declaring it. A pre-trust token cannot access trusted
//! actions — the type itself encodes the gate.

/// Trust state of a configuration source. Untrusted project hooks (and
/// untrusted project skills, once the skill trust-gate lands) are marked
/// skipped rather than silently dropped, so the user can see what was
/// elided. Maps to workspace trust (a project source is Untrusted until
/// the user acknowledges it; user/managed sources are Trusted by default).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustState {
    /// User-level or local: trusted by virtue of being on the user's machine.
    Trusted,
    /// Project-level, not yet acknowledged by the user via a trust prompt.
    Untrusted,
    /// User explicitly trusted this project (slash command or flag).
    Acknowledged,
}
