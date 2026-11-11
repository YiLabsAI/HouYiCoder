//! What a permission mode decides on its own, once every rule, safety check,
//! and stored consent has declined the call. The policies are pure functions
//! of the request and its side-effect level, so they can be read and tested
//! apart from the gate that dispatches to them.

use crate::decision::{AllowReason, AskReason, AskSource, Decision};
use crate::mode::{PermissionMode, ToolRequest};
use crate::pipeline::mode_default::MODE_DEFAULT;
use crate::side_effect::side_effect_for;
use houyicoder_api::sandbox::SideEffect;

/// A per-mode strategy that decides Allow / Ask / Deny for a tool request when
/// no rule, safety check, or consent matched. Each mode has its own policy
/// struct so the behavior is isolated and testable. The dispatch in mode_default
/// picks the struct for the current mode.
///
/// The policy is a pure function of the request and its pre-computed
/// side-effect level. It performs no I/O, keeps no mutable state, and runs no
/// classifier. The pipeline (rules, safety, compound, destructive, consent)
/// runs before the policy; the policy only handles the residual case where
/// nothing else decided.
pub trait ModePolicy {
    /// The verdict for a request with the given side-effect level.
    fn decide(&self, req: &ToolRequest, se: SideEffect) -> Decision;
}

/// Manual mode: least-privilege. A pure read auto-allows; every other call
/// escalates to Ask. A tool name the side-effect table does not know is
/// judged by what the tool declares: only a call the tool marks read-only
/// and non-destructive is treated as a pure read, so an unknown remote tool
/// asks instead of inheriting the read row.
pub struct DefaultPolicy;

impl ModePolicy for DefaultPolicy {
    fn decide(&self, req: &ToolRequest, se: SideEffect) -> Decision {
        match se {
            SideEffect::None => {
                if req.native_requires_approval {
                    Decision::Ask(mode_ask_reason("tool declares it needs approval"))
                } else if req.is_destructive || !req.is_read_only {
                    Decision::Ask(mode_ask_reason(
                        "manual mode asks before a tool that may change state",
                    ))
                } else {
                    Decision::Allow(AllowReason::ModeDefault)
                }
            }
            _ => Decision::Ask(mode_ask_reason("manual mode asks before this side effect")),
        }
    }
}

/// Auto mode: execution and filesystem writes are allowed (the destructive
/// and compound gates in the pipeline already caught dangerous commands);
/// network still escalates to Ask. A name the side-effect table does not
/// know is allowed only when the tool declares its effects reversible and
/// does not itself ask: a tool that keeps the trait's destructive default
/// asks, because no pipeline stage classified it.
pub struct AutoPolicy;

impl ModePolicy for AutoPolicy {
    fn decide(&self, req: &ToolRequest, se: SideEffect) -> Decision {
        match se {
            SideEffect::Exec => Decision::Allow(AllowReason::ModeDefault),
            SideEffect::Filesystem => Decision::Allow(AllowReason::ModeDefault),
            SideEffect::None => {
                if req.native_requires_approval {
                    Decision::Ask(mode_ask_reason("tool declares it needs approval"))
                } else if req.is_destructive {
                    Decision::Ask(mode_ask_reason("auto mode asks before a destructive tool"))
                } else {
                    Decision::Allow(AllowReason::ModeDefault)
                }
            }
            _ => Decision::Ask(mode_ask_reason("auto mode asks before network egress")),
        }
    }
}

/// Build the reason the mode-default validator attaches to an Ask it produces.
/// Every mode-default ask shares the same source and validator name; only the
/// one-sentence detail varies.
fn mode_ask_reason(detail: &str) -> AskReason {
    AskReason {
        source: AskSource::ToolNative,
        validator: MODE_DEFAULT,
        detail: detail.into(),
        containment_note: None,
    }
}

/// The per-mode default policy when no rule matches, no safety check fires,
/// and no consent is stored. Dispatches to the ModePolicy struct for the
/// given mode. The side-effect level is computed from the tool name and
/// drives the baseline: under Manual, Exec, Network, and Filesystem escalate
/// to Ask, and a name the table leaves unclassified asks unless the tool
/// declares itself read-only and non-destructive; Auto relaxes Exec and
/// Filesystem to Allow so the autonomous edit loop is usable (the pipeline's
/// destructive and compound gates already caught dangerous commands), asks
/// before Network, and asks before an unclassified name the tool declares
/// destructive or must approve. The trait defaults assume a tool mutates and
/// is destructive, so an unknown remote tool asks rather than running silently.
pub fn mode_default(mode: PermissionMode, req: &ToolRequest) -> Decision {
    let se = side_effect_for(req.tool_name);
    match mode {
        // Manual: ask unless the call is read-only and non-destructive.
        PermissionMode::Manual => DefaultPolicy.decide(req, se),
        // Auto: allow classified exec and filesystem; ask network and an
        // unclassified destructive or native-approval call.
        PermissionMode::Auto => AutoPolicy.decide(req, se),
    }
}
