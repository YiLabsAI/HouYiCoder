//! Conversions from the engine reason types to the wire form. The wire types
//! live in the protocol crate; the engine types live here. The conversion is
//! one-way: the wire form drops the containment proof token (a frontend cannot
//! verify it and has no use for it), so a reverse construction is deliberately
//! not provided — rebuilding the proof from wire would demote it to decoration.

use crate::decision::{AllowReason, AskReason, AskSource, DenyReason, DenySource, Outcome};
use houyicoder_protocol::frontend::permission as wire;

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
            source: wire::AskSource::from(&r.source),
            validator: r.validator.into(),
            detail: r.detail.clone(),
            containment_note: r.containment_note.clone(),
        }
    }
}

impl From<&DenyReason> for wire::DenyReason {
    fn from(r: &DenyReason) -> Self {
        Self {
            source: wire::DenySource::from(&r.source),
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
mod tests {
    use super::*;

    /// Every ask source maps to a distinct wire variant and back through
    /// serialization, so the approval card reads the same source the engine
    /// produced. The wire Unknown variant has no engine counterpart (it is the
    /// forward-compat fallback), so it is not in the round-trip set.
    #[test]
    fn test_ask_source_round_trips() {
        for s in [
            AskSource::UserRule,
            AskSource::SystemSafety,
            AskSource::Detection,
            AskSource::ToolNative,
        ] {
            let w = wire::AskSource::from(&s);
            let json = serde_json::to_string(&w).expect("serialize");
            let back: wire::AskSource = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, w, "wire ask source round-trips for {s:?}");
        }
    }

    #[test]
    fn test_ask_reason_round_trips() {
        let r = AskReason {
            source: AskSource::Detection,
            validator: "destructive_command",
            detail: "rm needs confirmation".into(),
            containment_note: None,
        };
        let w = wire::AskReason::from(&r);
        assert_eq!(w.source, wire::AskSource::Detection);
        assert_eq!(w.validator, "destructive_command");
        assert_eq!(w.detail, "rm needs confirmation");
        // The wire form round-trips through serde losslessly.
        let json = serde_json::to_string(&w).expect("serialize");
        let back: wire::AskReason = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, w);
    }

    #[test]
    fn test_deny_reason_round_trips() {
        let r = DenyReason {
            source: DenySource::Headless,
            validator: "post_transform",
            detail: "headless mode".into(),
        };
        let w = wire::DenyReason::from(&r);
        assert_eq!(w.source, wire::DenySource::Headless);
        assert_eq!(w.validator, "post_transform");
    }

    #[test]
    fn test_allow_reason_flattens_containment() {
        // The proof token is unreachable from the wire side; the variant
        // flattens to the Containment label with no payload.
        let proof = crate::decision::FenceProof::new(
            &houyicoder_api::sandbox::Coverage::Fenced {
                writable_roots: vec![std::path::PathBuf::from("/ws")],
            },
            houyicoder_api::sandbox::SideEffect::Exec,
        )
        .expect("fenced exec yields a proof");
        let r = AllowReason::Containment(proof);
        assert_eq!(wire::AllowReason::from(&r), wire::AllowReason::Containment);
        assert_eq!(
            wire::AllowReason::from(&AllowReason::ModeDefault),
            wire::AllowReason::ModeDefault
        );
    }

    #[test]
    fn test_outcome_maps_to_effect() {
        assert_eq!(
            wire::PermissionEffect::from(Outcome::Allow),
            wire::PermissionEffect::Allow
        );
        assert_eq!(
            wire::PermissionEffect::from(Outcome::Deny),
            wire::PermissionEffect::Reject
        );
        assert_eq!(
            wire::PermissionEffect::from(Outcome::Ask),
            wire::PermissionEffect::Ask
        );
    }
}
