use super::*;
use crate::rule::{Effect, Rule, RuleContent};
use crate::store::Scope;

fn npm_allow(action: &str, scope: Scope) -> Rule {
    Rule::with_content(action, RuleContent::Prefix("npm".into()), Effect::Allow)
        .unwrap()
        .with_scope(scope)
}

#[test]
fn test_same_rule_dedups() {
    let gate = DefaultModeGate::new_without_builtins();
    let rule = npm_allow("bash", Scope::Session);
    gate.add_rule(rule.clone());
    gate.add_rule(rule);
    assert_eq!(gate.rules().len(), 1, "dedup: in-memory should have 1 rule");
}

#[test]
fn test_case_insensitive_dedups() {
    let gate = DefaultModeGate::new_without_builtins();
    gate.add_rule(npm_allow("Bash", Scope::Session));
    gate.add_rule(npm_allow("bash", Scope::Session));
    assert_eq!(
        gate.rules().len(),
        1,
        "case-insensitive action is the same rule"
    );
}

#[test]
fn test_scope_distinguishes_rules() {
    // scope is part of rule identity: a Session rule and a Builtin rule
    // with the same action + content + effect are two distinct durable
    // directives (different destinations, different lifetimes). Dedup must
    // keep both. This is the one case the store layer cannot cover on its
    // own — read_envelope stamps every rule in a file with that file's
    // scope, so a single store.add call never sees two scopes at once. The
    // gate's in-memory list (union across scopes) is the only place a
    // cross-scope dedup could wrongly collapse; guard it here.
    //
    // Real failure this guards: a user holds a Project-scope bash(npm:*)
    // Allow, then authorizes the same rule User-scope ("all projects").
    // If scope were not part of identity, Rule::same_as would judge them
    // equal, the second add would no-op, and the cross-project
    // authorization would silently never take effect — with no UI row to
    // signal it.
    let gate = DefaultModeGate::new_without_builtins();
    gate.add_rule(npm_allow("bash", Scope::Session));
    gate.add_rule(npm_allow("bash", Scope::Builtin));
    assert_eq!(gate.rules().len(), 2, "scope is part of rule identity");
}
