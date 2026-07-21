use super::*;
use crate::decision::Outcome;

#[test]
fn test_fenced_exec_auto_allows() {
    use crate::decision::{AllowReason, Decision};
    use houyicoder_api::sandbox::{Containment, Coverage, SideEffect};
    use std::path::PathBuf;

    struct StubFence;
    impl Containment for StubFence {
        fn coverage(&self) -> Coverage {
            Coverage::Fenced {
                writable_roots: vec![PathBuf::from("/ws")],
            }
        }
        fn would_block(&self, _effect: SideEffect) -> Option<String> {
            None
        }
    }

    // In Manual mode a plain exec asks at the mode-default validator (the
    // only mode-governed verdict). post_transform upgrades that Ask to
    // Allow(Containment) when fenced + auto_allow is on -- the fence covers
    // the call, so the gate does not ask a question the fence answers.
    // Relaxation is pinned explicitly ON so this test locks the mechanism,
    // not the default (the default flips to off in a later change).
    let g = DefaultModeGate::with_mode(PermissionMode::Manual)
        .with_containment(Arc::new(StubFence))
        .with_auto_allow_fenced_exec(true);
    let d = g.decide(&bash_req("ls"));
    assert!(
        matches!(d, Decision::Allow(AllowReason::Containment(_))),
        "fenced mode-default exec should auto-allow via Containment proof"
    );
}

#[test]
fn test_auto_allow_off_asks() {
    use houyicoder_api::sandbox::{Containment, Coverage, SideEffect};
    use std::path::PathBuf;

    struct StubFence;
    impl Containment for StubFence {
        fn coverage(&self) -> Coverage {
            Coverage::Fenced {
                writable_roots: vec![PathBuf::from("/ws")],
            }
        }
        fn would_block(&self, _effect: SideEffect) -> Option<String> {
            None
        }
    }

    // With the relaxation explicitly off (which equals the default once the
    // default flips to off), even a mode-default exec Ask survives: the
    // conservative baseline asks for every exec the fence would otherwise cover.
    let g = DefaultModeGate::with_mode(PermissionMode::Manual)
        .with_containment(Arc::new(StubFence))
        .with_auto_allow_fenced_exec(false);
    let d = g.decide(&bash_req("ls"));
    assert!(
        matches!(d.outcome(), Outcome::Ask),
        "auto-allow off should still Ask for a mode-default exec"
    );
}

#[test]
fn test_auto_allow_defaults_off() {
    use houyicoder_api::sandbox::{Containment, Coverage, SideEffect};
    use std::path::PathBuf;
    use std::sync::Arc;

    struct StubFence;
    impl Containment for StubFence {
        fn coverage(&self) -> Coverage {
            Coverage::Fenced {
                writable_roots: vec![PathBuf::from("/ws")],
            }
        }
        fn would_block(&self, _effect: SideEffect) -> Option<String> {
            None
        }
    }

    // No explicit auto_allow setting: the default must be off, so a fenced
    // mode-default exec asks rather than silently auto-allowing. Pins the
    // default; the four mechanism tests above set it explicitly to on, and
    // auto_allow_off_still_asks sets it explicitly to off (which now equals
    // the default).
    let g =
        DefaultModeGate::with_mode(PermissionMode::Manual).with_containment(Arc::new(StubFence));
    let d = g.decide(&bash_req("ls"));
    assert!(
        matches!(d.outcome(), Outcome::Ask),
        "default auto_allow_fenced_exec must be off -- a fenced exec asks"
    );
}

#[test]
fn test_unfenced_exec_still_asks() {
    use houyicoder_api::sandbox::{Containment, Coverage, SideEffect};

    struct StubUnfenced;
    impl Containment for StubUnfenced {
        fn coverage(&self) -> Coverage {
            Coverage::Unfenced
        }
        fn would_block(&self, _effect: SideEffect) -> Option<String> {
            None
        }
    }

    // No fence proof can be built when the coverage is Unfenced, so a
    // mode-default exec Ask survives even with auto_allow explicitly on.
    let g = DefaultModeGate::with_mode(PermissionMode::Manual)
        .with_containment(Arc::new(StubUnfenced))
        .with_auto_allow_fenced_exec(true);
    let d = g.decide(&bash_req("ls"));
    assert!(
        matches!(d.outcome(), Outcome::Ask),
        "unfenced exec should Ask (no proof can be built)"
    );
}

#[test]
fn test_write_tools_no_fence() {
    use houyicoder_api::sandbox::{Containment, Coverage, SideEffect};
    use std::path::PathBuf;

    struct StubFence;
    impl Containment for StubFence {
        fn coverage(&self) -> Coverage {
            Coverage::Fenced {
                writable_roots: vec![PathBuf::from("/ws")],
            }
        }
        fn would_block(&self, _effect: SideEffect) -> Option<String> {
            None
        }
    }

    let g = DefaultModeGate::with_mode(PermissionMode::Auto)
        .with_containment(Arc::new(StubFence))
        .with_auto_allow_fenced_exec(true);
    let edit_req = ToolRequest {
        tool_name: "edit",
        input: None,
        is_destructive: true,
        is_read_only: false,
        native_requires_approval: true,
    };
    let d = g.decide(&edit_req);
    assert!(
        !matches!(
            d,
            crate::decision::Decision::Allow(crate::decision::AllowReason::Containment(_))
        ),
        "write tool should not get fence credit"
    );
}

#[test]
fn test_fenced_never_relaxes_immune() {
    // The fence never relaxes an immune verdict. With the fenced-exec
    // relaxation explicitly ON, every Ask the pipeline produces must
    // survive except a mode-default Ask -- the only mode-governed verdict,
    // the only one the fence may relax. The corpus spans every immune
    // validator family: reads (Allow), protected-path, destructive, egress,
    // git-checkpoint, and a user ask-rule. Under a Fenced vs an Unfenced
    // containment the outcome is identical for each -- the fence never
    // silently changes an immune verdict. (The mode-default relaxation is
    // pinned separately by fenced_exec_auto_allows.) Relaxation is pinned
    // explicitly ON so this test locks the mechanism, not the default.
    use crate::Scope;
    use crate::rule::{Effect, Rule, RuleContent};
    use houyicoder_api::sandbox::{Containment, Coverage, SideEffect};
    use std::path::PathBuf;
    use std::sync::Arc;

    struct StubFence;
    impl Containment for StubFence {
        fn coverage(&self) -> Coverage {
            Coverage::Fenced {
                writable_roots: vec![PathBuf::from("/ws")],
            }
        }
        fn would_block(&self, _effect: SideEffect) -> Option<String> {
            None
        }
    }
    struct StubUnfenced;
    impl Containment for StubUnfenced {
        fn coverage(&self) -> Coverage {
            Coverage::Unfenced
        }
        fn would_block(&self, _effect: SideEffect) -> Option<String> {
            None
        }
    }

    let fenced = DefaultModeGate::with_mode(PermissionMode::Auto)
        .with_containment(Arc::new(StubFence))
        .with_auto_allow_fenced_exec(true);
    let unfenced = DefaultModeGate::with_mode(PermissionMode::Auto)
        .with_containment(Arc::new(StubUnfenced))
        .with_auto_allow_fenced_exec(true);
    // A user ask-rule: bash(deploy*) = Ask. User-named intent is immune to
    // the fence relaxation -- the gate must not silence it.
    let ask_rule = Rule::with_content("bash", RuleContent::Prefix("deploy".into()), Effect::Ask)
        .unwrap()
        .with_scope(Scope::Session);
    fenced.add_rule(ask_rule.clone());
    unfenced.add_rule(ask_rule);

    let corpus: [(&str, Outcome); 12] = [
        ("cat file", Outcome::Allow),
        ("ls", Outcome::Allow),
        ("rm -rf .git/", Outcome::Ask),
        ("rm -rf /tmp/x", Outcome::Ask),
        ("curl http://x", Outcome::Ask),
        ("wget http://x", Outcome::Ask),
        ("git push", Outcome::Ask),
        ("git fetch", Outcome::Ask),
        ("git commit -m x", Outcome::Ask),
        ("git rebase main", Outcome::Ask),
        ("cargo publish", Outcome::Ask),
        ("deploy prod", Outcome::Ask),
    ];
    for (cmd, want) in corpus {
        let of = fenced.decide(&bash_req(cmd)).outcome();
        let ou = unfenced.decide(&bash_req(cmd)).outcome();
        assert_eq!(of, want, "fenced `{cmd}` => {of:?}, want {want:?}");
        assert_eq!(ou, want, "unfenced `{cmd}` => {ou:?}, want {want:?}");
        assert_eq!(
            of, ou,
            "`{cmd}` differs between fenced and unfenced -- fence leaked into an immune verdict"
        );
    }
}
