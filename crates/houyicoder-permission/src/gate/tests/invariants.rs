//! Gate invariants: the ladder order + spec-matching, mode-insensitivity of
//! immune stages, the unbypassable headless fallback, thread-safety of
//! concurrent decide() callers, and the path-bounds ask/defer discipline.
//! These assert properties that always hold of the assembled gate, not of
//! any single validator's decision.

use crate::decision::{Decision, DenySource, Outcome};
use crate::gate::{DefaultModeGate, ModeGate};
use crate::mode::{PermissionMode, ToolRequest};
use crate::pipeline::{Immunity, Pipeline, Stage};

use super::bash_req;

/// The ladder is sorted by Stage with no inversions. Pins the discriminant
/// order so reordering the registry is caught here, not just at the call site.
#[test]
fn test_ladder_order_is_total() {
    let info = Pipeline::standard().describe();
    let stages: Vec<Stage> = info.iter().map(|v| v.stage).collect();
    let mut sorted = stages.clone();
    sorted.sort();
    assert_eq!(stages, sorted, "registry must be sorted by stage");
}

/// The registry matches the spec table row by row: stage, name, immunity, and
/// consent-overridable. rule_ask lives at UserAsk (an authoritative user
/// directive ahead of the safety and detection ladders, where the builtin
/// git-checkpoint seed rules surface). A later sprint splits git_checkpoint
/// into the kept git_discard detection validator; that sprint updates this
/// table, which is the point — the change is forced to surface here rather
/// than hide in construction order.
#[test]
fn test_ladder_matches_spec() {
    // (stage, name, immunity, consent_overridable)
    const SPEC: &[(Stage, &str, Immunity, bool)] = &[
        (Stage::RuleDeny, "rule_deny", Immunity::ModeImmune, false),
        (Stage::UserAsk, "rule_ask", Immunity::ModeImmune, true),
        (
            Stage::SystemSafety,
            "protected_path",
            Immunity::ModeImmune,
            false,
        ),
        (
            Stage::Detection,
            "destructive_command",
            Immunity::ModeImmune,
            true,
        ),
        (
            Stage::Detection,
            "git_checkpoint",
            Immunity::ModeImmune,
            true,
        ),
        (
            Stage::Detection,
            "network_egress",
            Immunity::ModeImmune,
            false,
        ),
        (
            Stage::Detection,
            "compound_command",
            Immunity::ModeImmune,
            true,
        ),
        (Stage::Detection, "path-bounds", Immunity::ModeImmune, true),
        (Stage::RuleAllow, "rule_allow", Immunity::ModeImmune, false),
        (
            Stage::Consent,
            "stored_consent",
            Immunity::ModeImmune,
            false,
        ),
        (
            Stage::ModeDefault,
            "mode_default",
            Immunity::ModeGoverned,
            true,
        ),
    ];
    let info = Pipeline::standard().describe();
    assert_eq!(info.len(), SPEC.len(), "validator count matches spec");
    for (i, (stage, name, imm, co)) in SPEC.iter().enumerate() {
        assert_eq!(info[i].stage, *stage, "stage at row {i}");
        assert_eq!(info[i].name, *name, "name at row {i}");
        assert_eq!(info[i].immunity, *imm, "immunity at row {i}");
        assert_eq!(info[i].consent_overridable, *co, "consent at row {i}");
    }
}

/// INV-1: a request that hits a mode-immune stage (0, 2, 3) produces the same
/// decision under Manual and Auto. Mode only governs the fallback at the end
/// of the ladder; it never reaches in front of a rule, a protected path, or a
/// detection check.
#[test]
fn test_immune_stages_ignore_mode() {
    // Stage 0 (rule deny): a deny rule rejects in both modes.
    let req = bash_req("ls -la");
    let manual = DefaultModeGate::with_mode(PermissionMode::Manual);
    let auto = DefaultModeGate::with_mode(PermissionMode::Auto);
    manual.add_rule(crate::rule::Rule::new("bash", crate::rule::Effect::Deny).unwrap());
    auto.add_rule(crate::rule::Rule::new("bash", crate::rule::Effect::Deny).unwrap());
    assert_eq!(
        manual.decide(&req).outcome(),
        auto.decide(&req).outcome(),
        "rule deny is mode-immune"
    );

    // Stage 3 (destructive): rm asks in both modes, before the fallback.
    let req = bash_req("rm -rf /tmp/x");
    let manual = DefaultModeGate::with_mode(PermissionMode::Manual);
    let auto = DefaultModeGate::with_mode(PermissionMode::Auto);
    assert_eq!(
        manual.decide(&req).outcome(),
        auto.decide(&req).outcome(),
        "destructive detection is mode-immune"
    );
    assert_eq!(
        manual.decide(&req).outcome(),
        Outcome::Ask,
        "destructive command asks"
    );
}

/// INV-2: the headless transform runs after the ladder, so a request that the
/// ladder escalates to Ask still lands as Deny under headless. No validator
/// early return can skip the fallback.
#[test]
fn test_headless_denies_after_ladder() {
    let req = bash_req("rm -rf /tmp/x");
    let g = DefaultModeGate::with_mode(PermissionMode::Auto).with_headless(true);
    let d = g.decide(&req);
    assert_eq!(
        d.outcome(),
        Outcome::Deny,
        "headless turns the ask into a deny"
    );
    match d {
        Decision::Deny(reason) => {
            assert_eq!(
                reason.source,
                DenySource::Headless,
                "the deny cites headless, not the detection validator"
            );
        }
        _ => panic!("expected a deny under headless"),
    }
}

#[test]
fn test_concurrent_gate_decide_safe() {
    // Concurrent decide() callers share the gate's internal Mutex on rules +
    // mode. No panic and no corruption: every decision is a valid variant.
    // Stress both the rules lock (decide_inner) and the mode lock (set_mode).
    use std::sync::Arc;
    use std::thread;
    let g = Arc::new(DefaultModeGate::with_mode(PermissionMode::Auto));
    let n_threads = 8;
    let per_thread = 500;
    let mut handles = vec![];
    for t in 0..n_threads {
        let g = g.clone();
        handles.push(thread::spawn(move || {
            for i in 0..per_thread {
                // egress -> Deny (exercises rules lock + decide path)
                drop(g.decide(&bash_req("curl http://x")));
                // safe -> Allow
                drop(g.decide(&bash_req("ls -la")));
                if i % 100 == 0 {
                    // toggle mode to stress the mode/history Mutex
                    g.set_mode(
                        if t % 2 == 0 {
                            PermissionMode::Manual
                        } else {
                            PermissionMode::Auto
                        },
                        "stress",
                    );
                }
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
}

/// A grep/glob path or pattern that canonicalizes outside the workspace +
/// authorized dirs surfaces an Ask at the gate's path-bounds pre-check, so the
/// user can grant it — instead of the tool's hard PathEscapes rejection. The
/// gate only asks (it does not judge in-bounds); an in-workspace path defers
/// to the pipeline (default mode, no Ask). Pins the Bug A step-3 wiring + the
/// "only ask, never judge" discipline (the network-posture precedent).
#[test]
fn test_outside_asks_inside_defers() {
    use crate::decision::Outcome;
    use houyicoder_api::sandbox::{Containment, Coverage, SideEffect};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    let root = std::env::temp_dir().join(format!("gate-bounds-root-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("mkdir root");
    let outside = std::env::temp_dir().join(format!("gate-bounds-out-{}", std::process::id()));
    std::fs::create_dir_all(&outside).expect("mkdir outside");
    let croot = std::fs::canonicalize(&root).unwrap();
    let coutside = std::fs::canonicalize(&outside).unwrap();

    struct BoundsFence {
        root: PathBuf,
    }
    impl Containment for BoundsFence {
        fn coverage(&self) -> Coverage {
            Coverage::Fenced {
                writable_roots: vec![self.root.clone()],
            }
        }
        fn would_block(&self, _e: SideEffect) -> Option<String> {
            None
        }
        fn boundary_root(&self) -> Option<Arc<Path>> {
            Some(Arc::from(self.root.clone().into_boxed_path()))
        }
        fn boundary_dirs(&self) -> Vec<PathBuf> {
            Vec::new()
        }
    }
    let g = DefaultModeGate::new_without_builtins().with_containment(Arc::new(BoundsFence {
        root: croot.clone(),
    }));

    // grep path outside root → Ask.
    let v = serde_json::json!({"path": coutside.to_string_lossy(), "pattern": "x"});
    let req = ToolRequest {
        tool_name: "grep",
        input: Some(&v),
        is_destructive: false,
        is_read_only: true,
        native_requires_approval: false,
    };
    assert!(
        matches!(g.decide(&req).outcome(), Outcome::Ask),
        "out-of-workspace grep path must Ask"
    );

    // glob pattern whose dir portion is outside root → Ask.
    let pat = format!("{}/*", coutside.to_string_lossy());
    let v = serde_json::json!({"pattern": pat});
    let req = ToolRequest {
        tool_name: "glob",
        input: Some(&v),
        is_destructive: false,
        is_read_only: true,
        native_requires_approval: false,
    };
    assert!(
        matches!(g.decide(&req).outcome(), Outcome::Ask),
        "glob pattern escaping the workspace must Ask"
    );

    // glob pattern with ** (the most common glob) whose dir portion is outside
    // root → Ask. The wildcard must be truncated before the dir extraction or
    // canonicalize fails on the literal ** and the gate defers.
    let pat = format!("{}/**/*.rs", coutside.to_string_lossy());
    let v = serde_json::json!({"pattern": pat});
    let req = ToolRequest {
        tool_name: "glob",
        input: Some(&v),
        is_destructive: false,
        is_read_only: true,
        native_requires_approval: false,
    };
    assert!(
        matches!(g.decide(&req).outcome(), Outcome::Ask),
        "glob ** pattern escaping the workspace must Ask (wildcard truncated before dir extraction)"
    );

    // grep path inside root → not Ask (defers to the pipeline).
    let inside = root.join("a.txt");
    std::fs::write(&inside, b"x").ok();
    let v = serde_json::json!({"path": inside.to_string_lossy()});
    let req = ToolRequest {
        tool_name: "grep",
        input: Some(&v),
        is_destructive: false,
        is_read_only: true,
        native_requires_approval: false,
    };
    let d = g.decide(&req);
    assert!(
        !matches!(d.outcome(), Outcome::Ask),
        "in-workspace grep path does not ask: {d:?}"
    );

    // A gate with NO containment defers (path_outside_ask returns None at the
    // containment None early-return) — the tool's confine_path is the
    // backstop, the gate does not guess in-bounds. Pins fail-closed: no fence
    // info means no Ask, never an Allow guess.
    let g2 = DefaultModeGate::new_without_builtins();
    let v = serde_json::json!({"path": coutside.to_string_lossy(), "pattern": "x"});
    let req = ToolRequest {
        tool_name: "grep",
        input: Some(&v),
        is_destructive: false,
        is_read_only: true,
        native_requires_approval: false,
    };
    let d2 = g2.decide(&req);
    assert!(
        !matches!(d2.outcome(), Outcome::Ask),
        "no containment → gate does not ask (confine_path is the backstop): {d2:?}"
    );

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&outside).ok();
}
