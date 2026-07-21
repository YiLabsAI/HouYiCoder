//! The consent stage: a stored consent for the exact call upgrades an Ask to
//! an Allow. This is the fallback consent check that fires when no rule or
//! detection validator produced a verdict — the consent-overridable checks
//! earlier in the ladder each consult the consent store themselves, so by the
//! time this validator runs, consent is the only thing left that can decide.

use crate::decision::{AllowReason, Decision};
use crate::mode::ToolRequest;
use crate::pipeline::{GateCtx, Immunity, Stage, Validator, consent_allows};

/// A stored consent for the exact call. Returns Allow when a consent hit is
/// recorded, otherwise falls through to the mode default.
pub struct StoredConsentValidator;

impl Validator for StoredConsentValidator {
    fn name(&self) -> &'static str {
        "stored_consent"
    }
    fn stage(&self) -> Stage {
        Stage::Consent
    }
    fn immunity(&self) -> Immunity {
        Immunity::ModeImmune
    }
    fn consent_overridable(&self) -> bool {
        false
    }
    fn check(&self, req: &ToolRequest<'_>, ctx: &GateCtx<'_>) -> Option<Decision> {
        if consent_allows(req, ctx) {
            Some(Decision::Allow(AllowReason::Consent))
        } else {
            None
        }
    }
}
