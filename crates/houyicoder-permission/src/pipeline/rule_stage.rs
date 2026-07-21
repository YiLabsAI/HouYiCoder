//! The rule stage: deny-rules win outright, then the ask-rule fires ahead
//! of the safety and detection checks (a user-configured ask rule is an
//! authoritative "ask me" directive), then the allow-rule arm. The effect is
//! pre-computed once at the top of the ladder and read here, so the two arms
//! are two validators that fire on mutually exclusive effect values rather
//! than one validator with an inner match. The ask-rule lives at UserAsk so
//! the builtin seed rules (a git checkpoint ask) surface before the
//! detection + safety ladders run.

use crate::decision::{AllowReason, AskReason, AskSource, Decision, DenyReason, DenySource};
use crate::mode::ToolRequest;
use crate::pipeline::{GateCtx, Immunity, Stage, Validator, consent_allows};

/// A deny rule the user configured. Wins outright; no consent override.
pub struct RuleDenyValidator;

impl Validator for RuleDenyValidator {
    fn name(&self) -> &'static str {
        "rule_deny"
    }
    fn stage(&self) -> Stage {
        Stage::RuleDeny
    }
    fn immunity(&self) -> Immunity {
        Immunity::ModeImmune
    }
    fn consent_overridable(&self) -> bool {
        false
    }
    fn check(&self, _req: &ToolRequest<'_>, ctx: &GateCtx<'_>) -> Option<Decision> {
        use crate::rule::Effect;
        if ctx.effect == Some(Effect::Deny) {
            Some(Decision::Deny(DenyReason {
                source: DenySource::UserRule,
                validator: self.name(),
                detail: "a deny rule matched this call".into(),
            }))
        } else {
            None
        }
    }
}

/// An ask rule the user configured (including the builtin seed rules). The
/// ask fires at UserAsk, ahead of the safety and detection ladders: a user
/// ask rule is an authoritative directive, not a heuristic the detection
/// layer could override. A stored consent for the exact call upgrades the ask
/// to an allow; otherwise the call escalates.
pub struct RuleAskValidator;

impl Validator for RuleAskValidator {
    fn name(&self) -> &'static str {
        "rule_ask"
    }
    fn stage(&self) -> Stage {
        Stage::UserAsk
    }
    fn immunity(&self) -> Immunity {
        Immunity::ModeImmune
    }
    fn consent_overridable(&self) -> bool {
        true
    }
    fn check(&self, req: &ToolRequest<'_>, ctx: &GateCtx<'_>) -> Option<Decision> {
        use crate::rule::Effect;
        if ctx.effect != Some(Effect::Ask) {
            return None;
        }
        if consent_allows(req, ctx) {
            return Some(Decision::Allow(AllowReason::Consent));
        }
        Some(Decision::Ask(AskReason {
            source: AskSource::UserRule,
            validator: self.name(),
            detail: "an ask rule matched this call".into(),
            containment_note: None,
        }))
    }
}

/// An allow rule the user configured. The compound-command check runs earlier
/// in the ladder, so by the time this fires the call is either compound-safe
/// or consented, and the allow is unconditional.
pub struct RuleAllowValidator;

impl Validator for RuleAllowValidator {
    fn name(&self) -> &'static str {
        "rule_allow"
    }
    fn stage(&self) -> Stage {
        Stage::RuleAllow
    }
    fn immunity(&self) -> Immunity {
        Immunity::ModeImmune
    }
    fn consent_overridable(&self) -> bool {
        false
    }
    fn check(&self, _req: &ToolRequest<'_>, ctx: &GateCtx<'_>) -> Option<Decision> {
        use crate::rule::Effect;
        if ctx.effect == Some(Effect::Allow) {
            Some(Decision::Allow(AllowReason::UserRule))
        } else {
            None
        }
    }
}
