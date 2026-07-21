use super::*;
use crate::decision::Outcome;
use crate::rule::Effect;

#[test]
fn test_git_ops_asks_default() {
    // Dangerous-but-recoverable local git history ops Ask by default (the human
    // checkpoint). git status / git log are not checkpoint ops and Allow.
    let g = DefaultModeGate::with_mode(PermissionMode::Auto);
    for cmd in [
        "git commit -m x",
        "git rebase main",
        "git reset --hard HEAD~1",
        "git tag v1",
    ] {
        assert!(
            g.decide(&bash_req(cmd)).outcome() == Outcome::Ask,
            "`{cmd}` should Ask"
        );
    }
    for cmd in ["git status", "git log --oneline", "git diff"] {
        assert!(
            g.decide(&bash_req(cmd)).outcome() == Outcome::Allow,
            "`{cmd}` should Allow (not a checkpoint op)"
        );
    }
}

#[test]
fn test_git_ops_off_allows() {
    // Disabling the git-checkpoint builtin rules (the /permission toggle) lets
    // git history ops fall through to the mode default, which Allows in Auto.
    let g = DefaultModeGate::with_mode(PermissionMode::Auto);
    g.set_git_checkpoint_enabled(false);
    assert!(matches!(
        g.decide(&bash_req("git commit -m x")).outcome(),
        Outcome::Allow
    ));
    assert!(matches!(
        g.decide(&bash_req("git rebase main")).outcome(),
        Outcome::Allow
    ));
}

#[test]
fn test_git_session_consent_skips() {
    // A session-scope allow rule for "git commit" shadows the builtin ask rule
    // for that subcommand this session; other git history ops still Ask. The
    // consent is command-level (prefix), not exact-args, so a different commit
    // message still matches.
    use crate::Scope;
    use crate::rule::{Rule, RuleContent};
    let g = DefaultModeGate::with_mode(PermissionMode::Auto);
    g.add_rule(
        Rule::with_content(
            "bash",
            RuleContent::Prefix("git commit".into()),
            Effect::Allow,
        )
        .unwrap()
        .with_scope(Scope::Session),
    );
    assert!(matches!(
        g.decide(&bash_req("git commit -m anything")).outcome(),
        Outcome::Allow
    ));
    assert!(matches!(
        g.decide(&bash_req("git rebase main")).outcome(),
        Outcome::Ask
    ));
}

#[test]
fn test_git_wrapped_interpreter_asks() {
    // A git history op wrapped in an interpreter (bash -c "git commit") is
    // unwrapped and caught by the detection validator — the builtin prefix
    // rule only matches the direct form, so the wrapped form needs detection.
    let g = DefaultModeGate::with_mode(PermissionMode::Auto);
    assert!(matches!(
        g.decide(&bash_req("bash -c \"git commit -m x\"")).outcome(),
        Outcome::Ask
    ));
}

#[test]
fn test_git_allow_rule_skips() {
    // An explicit durable Allow rule for git commit shadows the builtin ask
    // (last-match wins); a different git op without a matching rule still Asks.
    use crate::rule::RuleContent;
    let g = DefaultModeGate::with_mode(PermissionMode::Auto);
    g.add_rule(
        Rule::with_content(
            "bash",
            RuleContent::Prefix("git commit".into()),
            Effect::Allow,
        )
        .unwrap(),
    );
    assert!(matches!(
        g.decide(&bash_req("git commit -m x")).outcome(),
        Outcome::Allow
    ));
    assert!(matches!(
        g.decide(&bash_req("git rebase main")).outcome(),
        Outcome::Ask
    ));
}

#[test]
fn test_consent_via_trait_arc() {
    // The server adds a session-scope allow rule through Arc<dyn ModeGate>;
    // verify the trait dispatch reaches the real rule set, not the no-op
    // default.
    use crate::Scope;
    use crate::rule::{Rule, RuleContent};
    use std::sync::Arc;
    let g: Arc<dyn ModeGate> = Arc::new(DefaultModeGate::with_mode(PermissionMode::Auto));
    g.add_rule(
        Rule::with_content(
            "bash",
            RuleContent::Prefix("git commit".into()),
            Effect::Allow,
        )
        .unwrap()
        .with_scope(Scope::Session),
    );
    assert!(matches!(
        g.decide(&bash_req("git commit -m anything")).outcome(),
        Outcome::Allow
    ));
    assert!(matches!(
        g.decide(&bash_req("git rebase main")).outcome(),
        Outcome::Ask
    ));
}

#[test]
fn test_git_discard_forms_ask() {
    // Working-tree / history discards Ask in Auto (the recoverable-undo
    // snapshot fires before the command runs, then the human checkpoint
    // gates it). A bare git checkout <branch> switch is NOT a discard.
    let g = DefaultModeGate::with_mode(PermissionMode::Auto);
    for cmd in [
        "git checkout .",
        "git checkout -- file.rs",
        "git checkout -f main",
        "git restore file.rs",
        "git clean -fd",
        "git stash drop",
        "git branch -D feature",
        "git push --force origin main",
    ] {
        assert!(
            g.decide(&bash_req(cmd)).outcome() == Outcome::Ask,
            "{cmd} should Ask (discard form)"
        );
    }
    // Switches + non-destructive ops Allow.
    for cmd in ["git checkout main", "git switch feature", "git status"] {
        assert!(
            g.decide(&bash_req(cmd)).outcome() == Outcome::Allow,
            "{cmd} should Allow (not a discard)"
        );
    }
    // stash apply / list, branch create, restore --staged-only are NOT discards.
    assert!(matches!(
        g.decide(&bash_req("git stash apply")).outcome(),
        Outcome::Allow
    ));
    assert!(matches!(
        g.decide(&bash_req("git stash list")).outcome(),
        Outcome::Allow
    ));
    assert!(matches!(
        g.decide(&bash_req("git branch feature")).outcome(),
        Outcome::Allow
    ));
    assert!(matches!(
        g.decide(&bash_req("git restore --staged file.rs"))
            .outcome(),
        Outcome::Allow
    ));
}
