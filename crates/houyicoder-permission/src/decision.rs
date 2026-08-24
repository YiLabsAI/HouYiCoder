//! The gate verdict and its structured reason. Every decision variant
//! carries a reason so the approval card, the tracing span, and the audit
//! record all read the same value instead of reconstructing it from which
//! validator fired. Tests and call sites that only care about allow versus
//! ask versus deny compare Outcome via Decision::outcome().
//!
//! FenceProof is the proof token for AllowReason::Containment. It is unused
//! in the library build until the sandbox contract wires the Containment
//! trait; the per-item allow keeps it quiet without weakening the verdict
//! types the gate already produces. Coverage moved to the api sandbox port.

use serde::{Deserialize, Serialize};

use houyicoder_api::sandbox::{Coverage, SideEffect};

/// The outcome of a permission decision, stripped of its reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Allow,
    Ask,
    Deny,
}

/// The gate verdict for one tool request. Every variant carries a structured
/// reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow(AllowReason),
    Ask(AskReason),
    Deny(DenyReason),
}

impl Decision {
    /// The verdict without its reason.
    pub fn outcome(&self) -> Outcome {
        match self {
            Decision::Allow(_) => Outcome::Allow,
            Decision::Ask(_) => Outcome::Ask,
            Decision::Deny(_) => Outcome::Deny,
        }
    }
}

/// Where an approval prompt came from. The source drives the wording and the
/// immunity class: user rules and system safety checks are authoritative
/// regardless of mode; the tool's own approval flag is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AskSource {
    /// An ask rule the user configured, including rules shipped as builtin
    /// defaults.
    UserRule,
    /// A protected path the agent must never write to silently.
    SystemSafety,
    /// A deterministic heuristic that flagged the call as risky.
    Detection,
    /// The tool itself declared that it needs approval.
    ToolNative,
}

/// Why a call was escalated to the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskReason {
    pub source: AskSource,
    /// The validator that produced the verdict. A stable identifier used as a
    /// metrics bucket key and in the audit record.
    pub validator: &'static str,
    /// One sentence the user reads.
    pub detail: String,
    /// A note from the containment layer when the fence is expected to reject
    /// the call even after the user consents. Purely informational: the gate
    /// never turns this into a rejection.
    pub containment_note: Option<String>,
}

/// Why a call was rejected without asking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DenyReason {
    pub source: DenySource,
    pub validator: &'static str,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DenySource {
    /// A deny rule the user configured.
    UserRule,
    /// Headless operation turned an approval prompt into a rejection because
    /// there is no one to answer it.
    Headless,
}

/// Why a call was allowed. Not serializable as is: the containment variant
/// carries a proof token whose wire form is flattened in the protocol crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllowReason {
    UserRule,
    Consent,
    /// The containment layer proved the call is fenced, so the coarse approval
    /// was not needed. The token is the proof: it cannot be built without
    /// passing the coverage and side-effect check, so this variant cannot be
    /// spelled for a call the fence does not cover.
    Containment(FenceProof),
    ModeDefault,
}

/// Evidence that a call is covered by the execution fence. The field is
/// private and the only constructor is the checked one below, so holding a
/// value of this type is itself the proof. That is what makes the rule a
/// compile-time property rather than a convention: an allow decision cannot
/// claim fence coverage without a value only the checked constructor hands out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FenceProof {
    fenced_root_count: usize,
}

impl FenceProof {
    /// Build the proof, or None when the call is not eligible. Eligibility is
    /// exactly two conditions: the fence covers the call, and the call is an
    /// execution rather than a direct file write. File-writing tools bypass
    /// the fence today, so they never qualify.
    pub fn new(coverage: &Coverage, effect: SideEffect) -> Option<Self> {
        match (coverage, effect) {
            (Coverage::Fenced { writable_roots, .. }, SideEffect::Exec) => Some(Self {
                fenced_root_count: writable_roots.len(),
            }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allow() -> Decision {
        Decision::Allow(AllowReason::ModeDefault)
    }
    fn ask() -> Decision {
        Decision::Ask(AskReason {
            source: AskSource::Detection,
            validator: "test",
            detail: "x".into(),
            containment_note: None,
        })
    }
    fn deny() -> Decision {
        Decision::Deny(DenyReason {
            source: DenySource::Headless,
            validator: "test",
            detail: "x".into(),
        })
    }

    #[test]
    fn test_outcome_maps_variants() {
        assert_eq!(allow().outcome(), Outcome::Allow);
        assert_eq!(ask().outcome(), Outcome::Ask);
        assert_eq!(deny().outcome(), Outcome::Deny);
    }

    /// A fenced exec yields a proof; an unfenced call or a non-exec side effect
    /// does not. Pins the C2 eligibility at the type level.
    #[test]
    fn test_fence_proof_eligibility() {
        let fenced = Coverage::Fenced {
            writable_roots: vec![std::path::PathBuf::from("/ws")],
        };
        assert!(FenceProof::new(&fenced, SideEffect::Exec).is_some());
        assert!(FenceProof::new(&Coverage::Unfenced, SideEffect::Exec).is_none());
        assert!(FenceProof::new(&fenced, SideEffect::Filesystem).is_none());
    }
}
