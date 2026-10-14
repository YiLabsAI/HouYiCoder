use super::*;
use crate::decision::{AskReason, DenyReason};

#[test]
fn test_counter_outcome_source_validator() {
    let counter = DecisionCounter::new();
    let ask = Decision::Ask(AskReason {
        source: AskSource::Detection,
        validator: "destructive_command",
        detail: "rm".into(),
        containment_note: None,
    });
    counter.inc(&ask);
    counter.inc(&ask);
    let deny = Decision::Deny(DenyReason {
        source: DenySource::Headless,
        validator: "post_transform",
        detail: "headless".into(),
    });
    counter.inc(&deny);
    let snap = counter.snapshot();
    // Two buckets: ask/detection/destructive_command = 2, deny/headless/post_transform = 1.
    assert_eq!(snap.len(), 2);
    let ask_bucket = snap
        .iter()
        .find(|b| b.outcome == "ask" && b.source == "detection")
        .expect("ask bucket");
    assert_eq!(ask_bucket.validator, "destructive_command");
    assert_eq!(ask_bucket.count, 2);
    let deny_bucket = snap
        .iter()
        .find(|b| b.outcome == "deny" && b.source == "headless")
        .expect("deny bucket");
    assert_eq!(deny_bucket.count, 1);
}
