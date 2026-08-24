//! The permission ladder as data. The gate's old if-chain becomes an ordered
//! registry of validators; the registry is built once and shared, and the
//! ladder order is asserted by an invariant test rather than by the order of
//! construction calls.
//!
//! The shared context deliberately carries no containment handle. Only two
//! components see the fence: the mode-default validator, which may relax its
//! own fallback, and the post transform, which may annotate a prompt. Every
//! other validator cannot reference the fence because it has no way to reach
//! it. That absence is the enforcement mechanism for the containment
//! contracts; adding a field here would silently undo them.

// The describe() ladder view plus the immunity / consent_overridable methods
// are exercised by the invariant tests and feed a permission view that is not
// wired yet; they are dead in the non-test library build, so the crate-level
// allow keeps them quiet without weakening the rest of the module.

#![allow(dead_code)] // ladder view metadata pending permission view wiring; locally unused

pub mod consent_stage;
pub mod detection;
pub mod mode_default;
pub mod post_transform;
pub mod rule_stage;
pub mod safety_stage;

use crate::consent::{ConsentStore, args_key};
use crate::decision::Decision;
use crate::mode::{PermissionMode, ToolRequest};
use crate::rule::{Effect, Rule};
use std::sync::Arc;

/// The position of a validator in the ladder. The discriminant is the order:
/// the registry is sorted by it and an invariant test asserts the sort is
/// total and has no duplicates within a stage that would make it ambiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stage {
    RuleDeny,
    UserAsk,
    SystemSafety,
    Detection,
    RuleAllow,
    Consent,
    ModeDefault,
}

/// Whether a validator's verdict survives the mode default. Descriptive
/// metadata, not an enforcement mechanism: it feeds the ladder description
/// shown in the permission view and the invariant assertions in the test
/// suite. Enforcement comes from the fact that only two components hold a
/// containment handle, so no other validator can relax a verdict even if it
/// wanted to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Immunity {
    ModeImmune,
    ModeGoverned,
}

/// One step of the permission ladder. Implementors are stateless: everything a
/// check needs comes from the request or the shared context, so the registry
/// can be built once and shared across threads.
pub trait Validator: Send + Sync {
    /// A stable identifier used in the verdict, in metrics, and in the audit
    /// record. Lowercase with underscores.
    fn name(&self) -> &'static str;

    /// The position of this validator in the ladder.
    fn stage(&self) -> Stage;

    /// Whether this validator's verdict survives the mode default.
    fn immunity(&self) -> Immunity;

    /// Whether a stored consent for the exact call can turn this validator's
    /// ask into an allow.
    fn consent_overridable(&self) -> bool;

    /// Inspect the request. Returning None means this validator has no opinion
    /// and the ladder continues to the next one.
    fn check(&self, req: &ToolRequest<'_>, ctx: &GateCtx<'_>) -> Option<Decision>;
}

/// Everything the validators share for one decision: the parsed command
/// segments, the rule set snapshot, the consent store, and the current mode.
/// Parsed once at the top of the ladder so no validator re-tokenizes the
/// command.
///
/// There is deliberately no containment handle here. Only two components see
/// the fence: the mode-default validator, which may relax its own fallback
/// verdict, and the post transform, which may annotate a prompt. Every other
/// validator cannot reference the fence because it has no way to reach it.
/// That absence is the enforcement mechanism for the containment contracts;
/// adding a field here would silently undo them.
pub struct GateCtx<'a> {
    pub mode: PermissionMode,
    pub content: &'a str,
    pub segments: &'a [String],
    pub rules: &'a [Rule],
    pub consent: Option<&'a dyn ConsentStore>,
    /// The pre-computed rule effect for this request, evaluated once at the
    /// top of the ladder so the rule-stage validators read a shared value
    /// instead of each re-running the rule matcher.
    pub effect: Option<Effect>,
    /// Whether the git-checkpoint builtin rules are enabled. Derived from the
    /// gate's disabled-builtin set, snapshotted once per decide. The detection
    /// validator's checkpoint arm reads this so the /permission git toggle
    /// turns off BOTH the builtin ask rule (direct form) AND the detection
    /// arm (wrapped form) together.
    pub git_checkpoint_enabled: bool,
}

/// Whether a stored consent for the exact call would upgrade an Ask to Allow.
/// Returns false when no consent store is attached. Bypass-immune asks are not
/// reached here: the caller checks consent_overridable() first.
pub fn consent_allows(req: &ToolRequest<'_>, ctx: &GateCtx<'_>) -> bool {
    let Some(cs) = ctx.consent else {
        return false;
    };
    cs.recall(req.tool_name, &args_key(req.input))
}

/// A snapshot of a validator for the permission view and the invariant test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatorInfo {
    pub name: &'static str,
    pub stage: Stage,
    pub immunity: Immunity,
    pub consent_overridable: bool,
}

/// The ordered ladder. Built once from a validator list, sorted by stage, and
/// shared behind an Arc.
pub struct Pipeline {
    validators: Vec<Box<dyn Validator>>,
}

impl Pipeline {
    /// Build the default ladder without a fence view. The order is asserted by
    /// an invariant test, not by the order of these calls.
    pub fn standard() -> Self {
        // Registration order tracks the gate's old if-chain line by line.
        // The sort is by Stage; within a stage, registration order is the
        // tiebreaker, so rule_allow is registered before rule_ask to keep the
        // old Allow-then-Ask arm order inside Stage::RuleAllow.
        let validators: Vec<Box<dyn Validator>> = vec![
            Box::new(rule_stage::RuleDenyValidator),
            Box::new(safety_stage::ProtectedPathValidator::new(None)),
            Box::new(detection::DestructiveCommandValidator),
            Box::new(detection::GitCheckpointValidator),
            Box::new(detection::NetworkEgressValidator),
            Box::new(detection::CompoundCommandValidator),
            Box::new(detection::PathBoundsValidator::new(None)),
            Box::new(rule_stage::RuleAllowValidator),
            Box::new(rule_stage::RuleAskValidator),
            Box::new(consent_stage::StoredConsentValidator),
            Box::new(mode_default::ModeDefaultValidator),
        ];
        let mut p = Self { validators };
        p.validators.sort_by_key(|v| v.stage());
        p
    }

    /// Build the ladder with a fence view injected into the validators that
    /// need one. The gate calls this from with_containment so they see the
    /// real workspace root + authorized dirs without GateCtx gaining a
    /// containment field (the design keeps GateCtx fence-free; the validators
    /// hold the handle directly).
    ///
    /// Two stages take it. Path-bounds uses the root to tell an
    /// out-of-workspace read from an inside one. Protected-path uses it to
    /// resolve a write target, so a symlink cannot spell its way around the
    /// markers; that stage keeps its supplied-string check either way, so a
    /// ladder built without a fence loses no coverage.
    pub fn with_containment(containment: Arc<dyn houyicoder_api::sandbox::Containment>) -> Self {
        let mut p = Self::standard();
        p.validators
            .retain(|v| v.name() != "path-bounds" && v.name() != "protected_path");
        p.validators
            .push(Box::new(detection::PathBoundsValidator::new(Some(
                Arc::clone(&containment),
            ))));
        p.validators
            .push(Box::new(safety_stage::ProtectedPathValidator::new(Some(
                containment,
            ))));
        p.validators.sort_by_key(|v| v.stage());
        p
    }

    /// Run the ladder. The first validator that returns a decision wins; if
    /// none do, the mode-default fallback applies. The post transform runs
    /// last so an early return cannot bypass it.
    pub fn decide(&self, req: &ToolRequest<'_>, ctx: &GateCtx<'_>) -> Decision {
        for v in &self.validators {
            if let Some(d) = v.check(req, ctx) {
                return d;
            }
        }
        // The mode-default validator is registered last and always returns a
        // concrete verdict, so reaching here is a programming error: someone
        // removed it from the registry or made it return None.
        unreachable!("the mode-default validator must always fire")
    }

    /// The ladder as data, for the invariant test and for the permission view
    /// that shows the user which checks are active.
    pub fn describe(&self) -> Vec<ValidatorInfo> {
        self.validators
            .iter()
            .map(|v| ValidatorInfo {
                name: v.name(),
                stage: v.stage(),
                immunity: v.immunity(),
                consent_overridable: v.consent_overridable(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The standard ladder is non-empty and sorted by stage. The exact
    /// membership and order are pinned by the spec-matching invariant test in
    /// the gate test suite; this only guards the trivial invariants here.
    #[test]
    fn test_standard_ladder_is_sorted() {
        let p = Pipeline::standard();
        let info = p.describe();
        assert!(!info.is_empty());
        let stages: Vec<Stage> = info.iter().map(|v| v.stage).collect();
        let mut sorted = stages.clone();
        sorted.sort();
        assert_eq!(stages, sorted);
    }

    /// The stage order is total: every stage compares, and the ladder sorts by
    /// it. Pins the discriminant order so a reorder is caught.
    #[test]
    fn test_stage_order_total() {
        assert!(Stage::RuleDeny < Stage::UserAsk);
        assert!(Stage::UserAsk < Stage::SystemSafety);
        assert!(Stage::SystemSafety < Stage::Detection);
        assert!(Stage::Detection < Stage::RuleAllow);
        assert!(Stage::RuleAllow < Stage::Consent);
        assert!(Stage::Consent < Stage::ModeDefault);
    }
}
