//! The permission layer that sits between the runner and a Tool. A PermissionMode
//! decides allow / ask / deny for a tool request; a durable Rule overrides the
//! mode default (the user-configurable whitelist that makes the autonomous mode
//! usable). GuardedTool wraps a Tool and routes every call through the gate, so
//! the core runner and the Tool trait stay untouched — the layer plugs in at
//! tool registration.
//!
//! The gate pipeline order: deny-rules win outright; then an ask- or
//! allow-rule (an ask-rule can be satisfied by stored consent, an allow-rule
//! still escalates an un-attestable compound shell command to Ask); then,
//! when no rule matched, the bypass-immune safety check (protected paths
//! like the version-control directory and shell rc files ask even in
//! Bypass), compound-command safety, stored consent, and the mode default. The
//! Default variant is gone: no-match falls straight through to the mode default,
//! which always returns a concrete Allow / Ask / Deny. Compound shell commands
//! are checked per-segment: any un-attestable segment (redirect, command
//! substitution) escalates the whole command to Ask.

mod compound;
mod consent;
mod decision;
mod gate;
mod git_discard;
mod guarded_tool;
mod heredoc;
mod metrics;
mod mode;
mod pipeline;
mod rule;
mod safety;
mod side_effect;
mod store;
mod wire;

#[cfg(test)]
#[path = "decide_bench_tests.rs"]
mod decide_bench_tests;

pub use compound::{compound_safe, is_attestable, split_compound};
pub use consent::{ConsentStore, InMemoryConsentStore, args_key};
pub use decision::{
    AllowReason, AskReason, AskSource, Decision, DenyReason, DenySource, FenceProof, Outcome,
};
pub use gate::{
    AutoPolicy, DefaultModeGate, DefaultPolicy, ModeGate, ModePolicy, classify_git_op, mode_default,
};
pub use guarded_tool::GuardedTool;
pub use metrics::{DecisionBucket, DecisionCounter, outcome_label};
pub use mode::{ModeChange, ModeError, PermissionMode, ToolRequest};
pub use rule::{
    Effect, GlobPattern, Rule, RuleContent, bash_always_allow_prefix, builtin_rule_id,
    builtin_rules, denied_agent_types, evaluate, input_content,
};
pub use safety::safety_check;
pub use side_effect::side_effect_for;
pub use store::{FileRuleStore, RuleStore, Scope, StoreError};
