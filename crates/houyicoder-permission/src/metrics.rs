//! In-process decision counter for the gate. Each decide() call increments a
//! bucket keyed by (outcome, source, validator) so a prompt-rate baseline
//! can be derived per source. The counter is a Mutex<HashMap> over &'static
//! str keys (no allocation on the hot path: the validator name is already
//! &'static str, and the source labels are matched from the reason
//! variant). A snapshot method exposes the buckets for the inspect / status
//! surface; there is no OTel or Prometheus backend — observability is the
//! in-process counter plus the inspect surface, not an external metrics
//! pipeline.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::decision::{AllowReason, AskSource, Decision, DenySource, Outcome};

/// One counter bucket: the outcome label, the source label, and the
/// validator name that produced the verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionBucket {
    pub outcome: &'static str,
    pub source: &'static str,
    pub validator: &'static str,
    pub count: u64,
}

/// The in-process decision counter. Thread-safe via an interior Mutex; the
/// lock is held only for the HashMap entry update, which is a few
/// microseconds per decide — the gate is called per tool call, not per
/// token, so the contention is bounded.
#[derive(Default)]
pub struct DecisionCounter {
    buckets: Mutex<HashMap<(&'static str, &'static str, &'static str), u64>>,
}

impl DecisionCounter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Increment the bucket the decision landed in. Best-effort: a poisoned
    /// lock is ignored (the gate keeps deciding; the metric is not a safety
    /// control).
    pub fn inc(&self, decision: &Decision) {
        let labels = decision_labels(decision);
        let mut buckets = match self.buckets.lock() {
            Ok(b) => b,
            Err(_) => return,
        };
        let entry = buckets
            .entry((labels.outcome, labels.source, labels.validator))
            .or_insert(0);
        *entry += 1;
    }

    /// A snapshot of all non-zero buckets, for the inspect / status surface.
    pub fn snapshot(&self) -> Vec<DecisionBucket> {
        let buckets = match self.buckets.lock() {
            Ok(b) => b,
            Err(_) => return Vec::new(),
        };
        let mut out: Vec<DecisionBucket> = buckets
            .iter()
            .map(|((outcome, source, validator), count)| DecisionBucket {
                outcome,
                source,
                validator,
                count: *count,
            })
            .collect();
        out.sort_by(|a, b| a.outcome.cmp(b.outcome).then(a.source.cmp(b.source)));
        out
    }
}

/// Named labels for a permission decision. The outcome is allow / ask / deny;
/// the source is the reason-source variant; the validator is the stable name the
/// gate attached. A tuple would let a return-order swap compile silently; the
/// named struct makes field assignment unambiguous.
pub(crate) struct DecisionLabels {
    pub outcome: &'static str,
    pub source: &'static str,
    pub validator: &'static str,
}

pub(crate) fn decision_labels(d: &Decision) -> DecisionLabels {
    match d {
        Decision::Allow(ar) => DecisionLabels {
            outcome: "allow",
            source: allow_source_label(ar),
            validator: allow_validator_label(ar),
        },
        Decision::Ask(ar) => DecisionLabels {
            outcome: "ask",
            source: ask_source_label(ar.source),
            validator: ar.validator,
        },
        Decision::Deny(dr) => DecisionLabels {
            outcome: "deny",
            source: deny_source_label(dr.source),
            validator: dr.validator,
        },
    }
}

fn allow_source_label(ar: &AllowReason) -> &'static str {
    match ar {
        AllowReason::UserRule => "user_rule",
        AllowReason::Consent => "consent",
        AllowReason::Containment(_) => "containment",
        AllowReason::ModeDefault => "mode_default",
    }
}

fn allow_validator_label(ar: &AllowReason) -> &'static str {
    match ar {
        AllowReason::UserRule => "rule_allow",
        AllowReason::Consent => "stored_consent",
        AllowReason::Containment(_) => "containment",
        AllowReason::ModeDefault => "mode_default",
    }
}

fn ask_source_label(s: AskSource) -> &'static str {
    match s {
        AskSource::UserRule => "user_rule",
        AskSource::SystemSafety => "system_safety",
        AskSource::Detection => "detection",
        AskSource::ToolNative => "tool_native",
    }
}

fn deny_source_label(s: DenySource) -> &'static str {
    match s {
        DenySource::UserRule => "user_rule",
        DenySource::Headless => "headless",
    }
}

/// The outcome label for a decision, without the source/validator. Used by
/// the tracing span field so a subscriber reads the outcome without the full
/// bucket.
pub fn outcome_label(d: &Decision) -> &'static str {
    match d.outcome() {
        Outcome::Allow => "allow",
        Outcome::Ask => "ask",
        Outcome::Deny => "deny",
    }
}

#[cfg(test)]
mod tests {
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
}
