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
