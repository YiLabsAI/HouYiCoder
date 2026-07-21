use super::*;
use crate::decision::Outcome;

#[test]
fn test_egress_detects_all_channels() {
    // Every egress channel the detector enumerates must Deny under the default
    // network-blocked profile. Covers the direct-tool, git-subcommand, and
    // publish branches that the per-channel tests above do not individually hit.
    let g = DefaultModeGate::with_mode(PermissionMode::Auto);
    let direct = [
        "wget http://x",
        "httpie get x",
        "scp f r:h",
        "rsync a b:/c",
        "ssh h",
        "sftp h",
        "ftp h",
        "nc h 80",
        "netcat h 80",
        "telnet h",
    ];
    for cmd in direct {
        assert!(
            g.decide(&bash_req(cmd)).outcome() == Outcome::Ask,
            "direct egress `{cmd}` should Ask"
        );
    }
    let git = ["git clone u", "git fetch", "git pull", "git ls-remote u"];
    for cmd in git {
        assert!(
            g.decide(&bash_req(cmd)).outcome() == Outcome::Ask,
            "git egress `{cmd}` should Ask"
        );
    }
    for cmd in ["cargo publish", "pip upload"] {
        assert!(
            g.decide(&bash_req(cmd)).outcome() == Outcome::Ask,
            "publish egress `{cmd}` should Ask"
        );
    }
}

#[test]
fn test_egress_channels_always_ask() {
    // C3: all egress channels Ask regardless of fence state.
    let g = DefaultModeGate::with_mode(PermissionMode::Auto);
    for cmd in ["wget http://x", "git fetch", "cargo publish", "pip upload"] {
        assert!(
            g.decide(&bash_req(cmd)).outcome() == Outcome::Ask,
            "egress `{cmd}` should Ask"
        );
    }
}

#[test]
fn test_interpreter_wrapped_egress_asks() {
    // Egress wrapped in an interpreter (bash -c "curl x") must still Ask —
    // the detector unwraps the -c inline code and scans it like a command.
    let g = DefaultModeGate::with_mode(PermissionMode::Auto);
    assert!(matches!(
        g.decide(&bash_req("bash -c \"curl http://evil.com\""))
            .outcome(),
        Outcome::Ask
    ));
    // A destructive wrapped in an interpreter must still Ask.
    assert!(matches!(
        g.decide(&bash_req("bash -c \"rm -rf /tmp/x\"")).outcome(),
        Outcome::Ask
    ));
    // Safe inline code is not falsely flagged.
    assert!(matches!(
        g.decide(&bash_req("bash -c \"echo hi\"")).outcome(),
        Outcome::Allow
    ));
}

#[test]
fn test_quoted_subcommand_egress_detected() {
    // A quoted subcommand (git "push") matches the bare word after
    // quote-stripping, so it is still detected as egress.
    let g = DefaultModeGate::with_mode(PermissionMode::Auto);
    assert!(matches!(
        g.decide(&bash_req("git \"push\" origin main")).outcome(),
        Outcome::Ask
    ));
}

#[test]
fn test_ask_carries_fence_note() {
    // When the fence is expected to reject an egress call, the gate attaches
    // the fence's verdict as a containment_note so the user sees that approval
    // will not help. The note is the fence's dynamic would_block output, not a
    // static string. Queried at the network layer (the call runs via bash, so
    // its side-effect classification is Exec, but the fence blocks egress).
    // Egress is immune to the fenced-exec relaxation, so the note appears in
    // the default config (auto_allow on) -- the fence is not an authority over
    // egress consent.
    use crate::decision::{AskReason, Decision};
    use houyicoder_api::sandbox::{Containment, Coverage, SideEffect};
    use std::path::PathBuf;
    use std::sync::Arc;

    struct StubBlockingFence;
    impl Containment for StubBlockingFence {
        fn coverage(&self) -> Coverage {
            Coverage::Fenced {
                writable_roots: vec![PathBuf::from("/ws")],
            }
        }
        fn would_block(&self, effect: SideEffect) -> Option<String> {
            match effect {
                SideEffect::Network => Some("egress is contained".into()),
                _ => None,
            }
        }
    }

    let g = DefaultModeGate::with_mode(PermissionMode::Auto)
        .with_containment(Arc::new(StubBlockingFence));
    let d = g.decide(&bash_req("curl http://x"));
    let AskReason {
        containment_note, ..
    } = match d {
        Decision::Ask(r) => r,
        other => panic!("expected Ask, got {other:?}"),
    };
    assert_eq!(containment_note.as_deref(), Some("egress is contained"));
}
