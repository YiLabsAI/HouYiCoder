//! Conversions from the engine reason types to the wire form. The wire types
//! live in the protocol crate; the engine types live here. The conversion is
//! one-way: the wire form drops the containment proof token (a frontend cannot
//! verify it and has no use for it), so a reverse construction is deliberately
//! not provided — rebuilding the proof from wire would demote it to decoration.

use crate::decision::{AllowReason, AskReason, AskSource, DenyReason, DenySource, Outcome};
use houyicoder_protocol::frontend::permission as wire;
use wire::AskSource as WireAskSource;
use wire::DenySource as WireDenySource;

impl From<&AskSource> for wire::AskSource {
    fn from(s: &AskSource) -> Self {
        match s {
            AskSource::UserRule => Self::UserRule,
            AskSource::SystemSafety => Self::SystemSafety,
            AskSource::Detection => Self::Detection,
            AskSource::ToolNative => Self::ToolNative,
        }
    }
}

impl From<&DenySource> for wire::DenySource {
    fn from(s: &DenySource) -> Self {
        match s {
            DenySource::UserRule => Self::UserRule,
            DenySource::Headless => Self::Headless,
        }
    }
}

impl From<&AskReason> for wire::AskReason {
    fn from(r: &AskReason) -> Self {
        Self {
            source: WireAskSource::from(&r.source),
            validator: r.validator.into(),
            detail: r.detail.clone(),
            containment_note: r.containment_note.clone(),
        }
    }
}

impl From<&DenyReason> for wire::DenyReason {
    fn from(r: &DenyReason) -> Self {
        Self {
            source: WireDenySource::from(&r.source),
            validator: r.validator.into(),
            detail: r.detail.clone(),
        }
    }
}

/// Flatten the engine allow reason to the wire form. The containment variant
/// carries a proof token the wire cannot represent, so it flattens to the
/// Containment label with no payload. The conversion is one-way.
impl From<&AllowReason> for wire::AllowReason {
    fn from(r: &AllowReason) -> Self {
        match r {
            AllowReason::UserRule => Self::UserRule,
            AllowReason::Consent => Self::Consent,
            AllowReason::Containment(_) => Self::Containment,
            AllowReason::ModeDefault => Self::ModeDefault,
        }
    }
}

/// The wire outcome of a decision, for the verdict-log entry. The engine
/// Outcome is already the right shape; this is a thin re-export so the service
/// boundary does not name the engine type when it only needs the label.
impl From<Outcome> for wire::PermissionEffect {
    fn from(o: Outcome) -> Self {
        match o {
            Outcome::Allow => Self::Allow,
            Outcome::Ask => Self::Ask,
            Outcome::Deny => Self::Reject,
        }
    }
}

#[cfg(test)]
mod tests;
