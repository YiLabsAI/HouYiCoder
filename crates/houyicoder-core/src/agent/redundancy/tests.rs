//! Tests for this module.

use super::*;
use serde_json::json;

fn calls<'a>(spec: &'a [(&'a str, serde_json::Value)]) -> Vec<(&'a str, &'a serde_json::Value)> {
    spec.iter().map(|(n, v)| (*n, v)).collect()
}

/// Re-reading the same file with no intervening write is the canonical
/// redundant smell (context-loss re-read). Flagged CrossBatch.
#[test]
fn test_cross_batch_no_write() {
    let mut t = RedundancyTracker::new();
    t.record("read", &json!({"file_path": "a.rs"}), false);
    t.check_batch(calls(&[("read", json!({"file_path": "a.rs"}))]));
    let recs = t.records();
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].kind, RedundancyKind::CrossBatch);
    assert_eq!(recs[0].tool, "read");
}

/// A write anywhere after the prior call means state changed — the
/// re-read is legitimate (fresh context), not redundant. Coarse
/// (per-path precision is a later refinement) but false-negative-safe.
#[test]
fn test_cross_batch_after_write() {
    let mut t = RedundancyTracker::new();
    t.record("read", &json!({"file_path": "a.rs"}), false);
    t.record("write", &json!({"file_path": "b.rs"}), false);
    t.check_batch(calls(&[("read", json!({"file_path": "a.rs"}))]));
    assert!(t.records().is_empty(), "write invalidated the re-read");
}

/// Retrying after a prior error is legit (the prior failed). Not flagged.
#[test]
fn test_retry_after_error() {
    let mut t = RedundancyTracker::new();
    t.record("read", &json!({"file_path": "a.rs"}), true);
    t.check_batch(calls(&[("read", json!({"file_path": "a.rs"}))]));
    assert!(t.records().is_empty(), "retry-after-error is not redundant");
}

/// Esc-interrupt then retry: the fallback Interrupted result carries
/// {"error":"interrupted by user"} so record sees is_error=true →
/// outcome=Error → the retry is not flagged. Pins the tracker side of the
/// dependency; the call-site side (is_error = o.get("error").is_some() at
/// pipeline.rs) keeps that json honest.
#[test]
fn test_retry_after_interrupt() {
    let mut t = RedundancyTracker::new();
    // The Interrupted fallback is is_error=true at the call site, so the
    // tracker records outcome=Error — same shape as test_retry_after_error.
    t.record("read", &json!({"file_path": "a.rs"}), true);
    t.check_batch(calls(&[("read", json!({"file_path": "a.rs"}))]));
    assert!(t.records().is_empty(), "Esc-then-retry is not redundant");
}

/// Different input (an offset change, a narrower pattern) is a fresh
/// call, not a duplicate. Not flagged.
#[test]
fn test_different_input_no_flag() {
    let mut t = RedundancyTracker::new();
    t.record("read", &json!({"file_path": "a.rs", "offset": 0}), false);
    t.check_batch(calls(&[(
        "read",
        json!({"file_path": "a.rs", "offset": 100}),
    )]));
    assert!(t.records().is_empty(), "different input is not a duplicate");
}

/// Two identical calls in one assistant message (one batch), before
/// either executes — the strongest signal. The ledger alone can't catch
/// it (neither has executed); the batch-local seen set does.
#[test]
fn test_same_batch_duplicate() {
    let mut t = RedundancyTracker::new();
    t.check_batch(calls(&[
        ("read", json!({"file_path": "a.rs"})),
        ("read", json!({"file_path": "a.rs"})),
    ]));
    let recs = t.records();
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].kind, RedundancyKind::SameBatch);
    assert_eq!(recs[0].gap, 0);
}

/// A write-tool record bumps last_global_write_seq so a subsequent
/// same-input call is not flagged.
#[test]
fn test_write_updates_seq() {
    let mut t = RedundancyTracker::new();
    t.record("read", &json!({"file_path": "a.rs"}), false);
    assert_eq!(t.last_global_write_seq, 0);
    t.record("edit", &json!({"file_path": "a.rs"}), false);
    assert_eq!(
        t.last_global_write_seq, 2,
        "write bumped the global write seq"
    );
    // Subsequent same-input read → not flagged (write happened).
    t.check_batch(calls(&[("read", json!({"file_path": "a.rs"}))]));
    assert!(t.records().is_empty());
}

/// The records ring is capped — a re-read storm (exactly the
/// pathological case the cap exists for) drops oldest, stays bounded.
#[test]
fn test_records_cap_drops_oldest() {
    let mut t = RedundancyTracker::new();
    // Record one read, then re-issue it REDUNDANCY_RECORDS_CAP + 5 times
    // across batches. Each re-issue (no intervening write) flags once.
    t.record("read", &json!({"file_path": "a.rs"}), false);
    for _ in 0..(REDUNDANCY_RECORDS_CAP + 5) {
        t.check_batch(calls(&[("read", json!({"file_path": "a.rs"}))]));
        // record() between batches so seq advances + ledger stays current;
        // otherwise the ledger entry's last_seq never moves and every
        // check_batch after the first hits a stale record.
        t.record("read", &json!({"file_path": "a.rs"}), false);
    }
    assert!(
        t.records().len() <= REDUNDANCY_RECORDS_CAP,
        "ring bounded: {}",
        t.records().len()
    );
    assert_eq!(
        t.records().len(),
        REDUNDANCY_RECORDS_CAP,
        "ring saturated to the cap"
    );
}

/// The ledger is bounded — a very long session of distinct calls does
/// not grow it without bound. Eviction drops the oldest entry.
#[test]
fn test_ledger_cap_evicts() {
    let mut t = RedundancyTracker::new();
    // Record LEDGER_CAP + 10 distinct reads (distinct paths → distinct
    // keys). The ledger must stay at or under LEDGER_CAP.
    for i in 0..(LEDGER_CAP + 10) {
        t.record("read", &json!({"file_path": format!("f{i}.rs")}), false);
    }
    assert!(
        t.ledger.len() <= LEDGER_CAP,
        "ledger bounded: {}",
        t.ledger.len()
    );
}

/// A same-input call re-issued after the prior one failed, with no
/// intervening write, is a blind retry — the reward signal. It is not
/// flagged redundant (the prior failed) but increments retry_after_error.
#[test]
fn test_blind_retry_counts() {
    let mut t = RedundancyTracker::new();
    let input = json!({"command": "cargo build"});
    t.record("bash", &input, true);
    t.check_batch(once(("bash", &input)));
    assert_eq!(t.retry_after_error(), 1);
}

/// A write between the failed call and the retry changes state, so the
/// retry is fresh (not blind) — retry_after_error does not increment.
#[test]
fn test_blind_retry_skips_write() {
    let mut t = RedundancyTracker::new();
    let input = json!({"command": "cargo build"});
    t.record("bash", &input, true);
    t.record("edit", &json!({"file_path": "src/lib.rs"}), false);
    t.check_batch(once(("bash", &input)));
    assert_eq!(t.retry_after_error(), 0);
}

/// A retry of a prior success (no write) is redundant, not a blind
/// retry — retry_after_error stays 0 and the redundant record is flagged.
#[test]
fn test_blind_retry_zero_redundant() {
    let mut t = RedundancyTracker::new();
    let input = json!({"file_path": "src/lib.rs"});
    t.record("read", &input, false);
    t.check_batch(once(("read", &input)));
    assert_eq!(t.retry_after_error(), 0);
    assert_eq!(
        t.records().len(),
        1,
        "prior success retry flagged redundant"
    );
}

/// A blind retry records the tool name in retry_tools so
/// observe_redundancy can append a per-tool MetaUser warning.
#[test]
fn test_blind_retry_tracks_tool() {
    let mut t = RedundancyTracker::new();
    let input = json!({"command": "cargo build"});
    t.record("bash", &input, true);
    t.check_batch(once(("bash", &input)));
    assert_eq!(t.retry_after_error(), 1);
    assert_eq!(t.retry_tools().len(), 1, "blind retry tool name recorded");
    assert_eq!(t.retry_tools()[0], "bash");
}

/// The retry_tools ring is capped — a blind-retry storm drops oldest,
/// stays bounded (same cap as the records ring).
#[test]
fn test_retry_cap_drops_oldest() {
    let mut t = RedundancyTracker::new();
    let input = json!({"command": "cargo build"});
    t.record("bash", &input, true);
    for _ in 0..(REDUNDANCY_RECORDS_CAP + 5) {
        t.check_batch(once(("bash", &input)));
        t.record("bash", &input, true);
    }
    assert!(
        t.retry_tools().len() <= REDUNDANCY_RECORDS_CAP,
        "retry_tools bounded: {}",
        t.retry_tools().len()
    );
}
