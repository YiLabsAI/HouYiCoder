//! Tests for this module.

use super::*;
use houyicoder_context::CachedBlockDecision;

fn ctx<'a>(age: u32, block_ref: Option<&'a str>, assembly_id: u64) -> RetentionContext<'a> {
    RetentionContext {
        age_in_turns: age,
        is_superseded: false,
        block_ref,
        assembly_id,
        mutating: true,
    }
}

fn measure<'a>(age: u32, block_ref: Option<&'a str>, assembly_id: u64) -> RetentionContext<'a> {
    RetentionContext {
        age_in_turns: age,
        is_superseded: false,
        block_ref,
        assembly_id,
        mutating: false,
    }
}

#[test]
fn test_hit_outlives_ttl() {
    let s = CachedPrefixState::new();
    // No turn recorded: not live (no cache to break).
    assert!(!s.cache_live());
    // A turn that hit the cache: live.
    s.record_turn(1_000, 800);
    assert!(s.cache_live());
    // A hit far past the declared TTL is still a hit: the server kept the
    // prefix, so the hold must not expire on the local clock.
    s.record_turn(1_000 + 8 * CACHE_TTL_MS, 800);
    assert!(s.cache_live());
    // A reported miss ends liveness.
    s.record_turn(3_000, 0);
    assert!(!s.cache_live());
}

#[test]
fn test_invalidate_clears_decisions() {
    let s = CachedPrefixState::new();
    s.record_turn(1_000, 800);
    s.store_decision("h1", RetentionDecision::Materialize, 0, 1);
    assert_eq!(s.generation(), 0);
    assert_eq!(s.decision_snapshot().len(), 1);
    s.invalidate();
    assert_eq!(s.generation(), 1);
    assert!(
        s.decision_snapshot().is_empty(),
        "map cleared on invalidate"
    );
    assert_eq!(s.last_cache_read_tokens(), 0, "baseline cleared");
    assert!(!s.cache_live());
    assert!(
        s.drain_if_dirty().is_none(),
        "a cleared map has nothing to record"
    );
}

#[test]
fn test_decision_stable_while_alive() {
    // The same block_ref served twice while the cache is live returns the
    // same decision — the prefix bytes do not flap as the block ages.
    let s = Arc::new(CachedPrefixState::new());
    s.record_turn(1_000, 800); // alive
    let policy = CacheLivenessRetentionPolicy::new(Arc::clone(&s));
    // First serve at age 1 (alive, conservative band: age 1 < 6 → Materialize).
    let d1 = policy.decide(&ctx(1, Some("h1"), 1));
    assert_eq!(d1, RetentionDecision::Materialize);
    // Second serve at age 8 (would Summarize under the conservative band,
    // but the stored decision holds while the cache is live).
    let d2 = policy.decide(&ctx(8, Some("h1"), 2));
    assert_eq!(d2, d1, "decision stable within generation while alive");
    // A different block at the same age gets its own decision.
    let d3 = policy.decide(&ctx(1, Some("h2"), 2));
    assert_eq!(d3, RetentionDecision::Materialize);
}

#[test]
fn test_miss_recomputes_policy() {
    // While the cache is live, a block_ref stores its decision. After a
    // reported miss, the stored decision is no longer consulted — the
    // aggressive band applies, so an age-3 block Summarizes.
    let s = Arc::new(CachedPrefixState::new());
    s.record_turn(1_000, 800);
    let policy = CacheLivenessRetentionPolicy::new(Arc::clone(&s));
    let _ = policy.decide(&ctx(3, Some("h1"), 1));
    s.record_turn(2_000, 0);
    let d = policy.decide(&ctx(3, Some("h1"), 2));
    assert_eq!(
        d,
        RetentionDecision::Summarize,
        "a reported miss overrides the stored decision"
    );
}

#[test]
fn test_hit_cancels_demote() {
    // A miss marks every block for an age-based recompute. If the server
    // rebuilds the prefix and reports a hit before the next assembly, the
    // hold is the correct choice again: demoting would break a live prefix.
    let s = Arc::new(CachedPrefixState::new());
    s.record_turn(1_000, 800);
    let policy = CacheLivenessRetentionPolicy::new(Arc::clone(&s));
    let _ = policy.decide(&ctx(3, Some("h1"), 1));
    s.record_turn(2_000, 0);
    s.record_turn(3_000, 700);
    let d = policy.decide(&ctx(3, Some("h1"), 2));
    assert_eq!(
        d,
        RetentionDecision::Materialize,
        "a rebuilt prefix cancels the pending demotion"
    );
}

#[test]
fn test_policy_evicts_superseded_block() {
    let s = Arc::new(CachedPrefixState::new());
    s.record_turn(1_000, 800);
    let policy = CacheLivenessRetentionPolicy::new(Arc::clone(&s));
    assert_eq!(
        policy.decide(&ctx(0, Some("h1"), 1)),
        RetentionDecision::Materialize
    );
    let mut c = ctx(0, Some("h1"), 2);
    c.is_superseded = true;
    assert_eq!(policy.decide(&c), RetentionDecision::Evict);
    assert!(
        s.decision_snapshot().is_empty(),
        "a superseded block's row is removed, not kept at Materialize"
    );
    assert!(
        s.drain_if_dirty().is_some(),
        "the removal is a durable map change"
    );
}

#[test]
fn test_live_uses_conservative_band() {
    // Alive: age 4 (>= aggressive materialize 2 but < conservative 6) →
    // Materialize under the conservative band, not Summarize.
    let s = Arc::new(CachedPrefixState::new());
    s.record_turn(1_000, 800);
    let policy = CacheLivenessRetentionPolicy::new(Arc::clone(&s));
    let d = policy.decide(&ctx(4, Some("h1"), 1));
    assert_eq!(d, RetentionDecision::Materialize);
}

/// When the provider did not report a cache-read count, touch_timestamp
/// advances the clock without zeroing a previously reported hit. The liveness
/// state is preserved — an unreported reading is unknown, not a confirmed
/// cache miss.
#[test]
fn test_touch_timestamp_preserves_liveness() {
    let s = CachedPrefixState::new();
    s.record_turn(1_000, 800);
    assert_eq!(s.last_cache_read_tokens(), 800);
    assert!(s.cache_live());
    s.touch_timestamp(2_000);
    assert_eq!(s.last_cache_read_tokens(), 800, "cache-read preserved");
    assert!(s.cache_live(), "still alive after touch");
    s.record_turn(2_100, 0);
    assert!(!s.cache_live(), "confirmed miss kills liveness");
}

/// Four held Materialize blocks past the live band trigger one batch
/// demotion on the next assembly. The younger block keeps its hold.
#[test]
fn test_batch_demotes_old() {
    let s = Arc::new(CachedPrefixState::new());
    s.record_turn(1_000, 800);
    let policy = CacheLivenessRetentionPolicy::new(Arc::clone(&s));
    for hash in ["h1", "h2", "h3", "h4", "young", "fresh"] {
        assert_eq!(
            policy.decide(&ctx(0, Some(hash), 1)),
            RetentionDecision::Materialize
        );
    }
    // One assembly sees four over-age held blocks and one young one.
    for hash in ["h1", "h2", "h3", "h4"] {
        assert_eq!(
            policy.decide(&ctx(7, Some(hash), 2)),
            RetentionDecision::Materialize,
            "the trigger fires after the lookup, so this assembly still holds"
        );
    }
    assert_eq!(
        policy.decide(&ctx(2, Some("young"), 2)),
        RetentionDecision::Materialize
    );
    assert_eq!(
        policy.decide(&ctx(0, Some("fresh"), 2)),
        RetentionDecision::Materialize
    );
    // The next assembly demotes the over-age blocks in one pass.
    for hash in ["h1", "h2", "h3", "h4"] {
        assert_eq!(
            policy.decide(&ctx(7, Some(hash), 3)),
            RetentionDecision::Summarize,
            "an over-age held block demotes at the batch boundary"
        );
    }
    assert_eq!(
        policy.decide(&ctx(2, Some("young"), 3)),
        RetentionDecision::Materialize,
        "the young block keeps its hold"
    );
    assert_eq!(
        policy.decide(&ctx(0, Some("fresh"), 3)),
        RetentionDecision::Materialize,
        "an age-0 read result is never demoted by the batch"
    );
}

/// Three over-age held blocks stay under the batch threshold, so the hold
/// survives another assembly.
#[test]
fn test_batch_below_threshold() {
    let s = Arc::new(CachedPrefixState::new());
    s.record_turn(1_000, 800);
    let policy = CacheLivenessRetentionPolicy::new(Arc::clone(&s));
    for hash in ["h1", "h2", "h3"] {
        assert_eq!(
            policy.decide(&ctx(0, Some(hash), 1)),
            RetentionDecision::Materialize
        );
    }
    for hash in ["h1", "h2", "h3"] {
        assert_eq!(
            policy.decide(&ctx(7, Some(hash), 2)),
            RetentionDecision::Materialize
        );
    }
    for hash in ["h1", "h2", "h3"] {
        assert_eq!(
            policy.decide(&ctx(7, Some(hash), 3)),
            RetentionDecision::Materialize,
            "below the batch threshold the hold survives"
        );
    }
}

/// A single materialized block at or above the byte threshold triggers the
/// batch on its own; one byte below it does not.
#[test]
fn test_bytes_trigger_batch() {
    let s = Arc::new(CachedPrefixState::new());
    s.record_turn(1_000, 800);
    let policy = CacheLivenessRetentionPolicy::new(Arc::clone(&s));
    assert_eq!(
        policy.decide(&ctx(0, Some("big"), 1)),
        RetentionDecision::Materialize
    );
    s.record_materialized_bytes("big", BATCH_DEMOTE_MIN_BYTES as usize - 1);
    assert_eq!(
        policy.decide(&ctx(7, Some("big"), 2)),
        RetentionDecision::Materialize,
        "the trigger fires after this lookup"
    );
    assert_eq!(
        policy.decide(&ctx(7, Some("big"), 3)),
        RetentionDecision::Materialize,
        "one byte below the threshold keeps the hold"
    );
    s.record_materialized_bytes("big", BATCH_DEMOTE_MIN_BYTES as usize);
    assert_eq!(
        policy.decide(&ctx(7, Some("big"), 4)),
        RetentionDecision::Materialize,
        "the trigger fires after this lookup"
    );
    assert_eq!(
        policy.decide(&ctx(7, Some("big"), 5)),
        RetentionDecision::Summarize,
        "the threshold triggers the batch"
    );
}

/// The batch boundary is the live materialize band: age 5 holds, age 6
/// triggers.
#[test]
fn test_batch_age_boundary() {
    let s = Arc::new(CachedPrefixState::new());
    s.record_turn(1_000, 800);
    let policy = CacheLivenessRetentionPolicy::new(Arc::clone(&s));
    for hash in ["h1", "h2", "h3", "h4"] {
        assert_eq!(
            policy.decide(&ctx(0, Some(hash), 1)),
            RetentionDecision::Materialize
        );
    }
    for hash in ["h1", "h2", "h3", "h4"] {
        assert_eq!(
            policy.decide(&ctx(5, Some(hash), 2)),
            RetentionDecision::Materialize
        );
    }
    for hash in ["h1", "h2", "h3", "h4"] {
        assert_eq!(
            policy.decide(&ctx(5, Some(hash), 3)),
            RetentionDecision::Materialize,
            "age 5 is still inside the live band"
        );
    }
    for hash in ["h1", "h2", "h3", "h4"] {
        assert_eq!(
            policy.decide(&ctx(6, Some(hash), 4)),
            RetentionDecision::Materialize,
            "the trigger fires after the lookup"
        );
    }
    for hash in ["h1", "h2", "h3", "h4"] {
        assert_eq!(
            policy.decide(&ctx(6, Some(hash), 5)),
            RetentionDecision::Summarize,
            "age 6 crosses the batch boundary"
        );
    }
}

/// A miss demotes every block in one assembly, not one block per turn.
#[test]
fn test_miss_demotes_all() {
    let s = Arc::new(CachedPrefixState::new());
    s.record_turn(1_000, 800);
    let policy = CacheLivenessRetentionPolicy::new(Arc::clone(&s));
    for hash in ["h1", "h2"] {
        assert_eq!(
            policy.decide(&ctx(0, Some(hash), 1)),
            RetentionDecision::Materialize
        );
    }
    s.record_turn(2_000, 0);
    for hash in ["h1", "h2"] {
        assert_eq!(
            policy.decide(&ctx(3, Some(hash), 2)),
            RetentionDecision::Summarize,
            "both blocks demote in the same assembly"
        );
    }
}

/// A held read that changes nothing leaves the durable map clean.
#[test]
fn test_held_read_stays_clean() {
    let s = Arc::new(CachedPrefixState::new());
    s.record_turn(1_000, 800);
    let policy = CacheLivenessRetentionPolicy::new(Arc::clone(&s));
    assert_eq!(
        policy.decide(&ctx(0, Some("h1"), 1)),
        RetentionDecision::Materialize
    );
    assert!(s.drain_if_dirty().is_some());
    assert_eq!(
        policy.decide(&ctx(0, Some("h1"), 2)),
        RetentionDecision::Materialize
    );
    assert!(
        s.drain_if_dirty().is_none(),
        "a held read is not a decision write"
    );
}

#[test]
fn test_dirty_on_change() {
    let s = CachedPrefixState::new();
    s.store_decision("h1", RetentionDecision::Materialize, 0, 1);
    assert!(s.drain_if_dirty().is_some(), "a new entry is a change");
    s.store_decision("h1", RetentionDecision::Materialize, 1, 1);
    assert!(
        s.drain_if_dirty().is_none(),
        "the same decision is not a change"
    );
    s.store_decision("h1", RetentionDecision::Summarize, 1, 1);
    assert!(
        s.drain_if_dirty().is_some(),
        "a different decision is a change"
    );
}

#[test]
fn test_restore_decisions_round_trip() {
    let s = CachedPrefixState::new();
    s.store_decision("h2", RetentionDecision::Summarize, 0, 1);
    s.store_decision("h1", RetentionDecision::Materialize, 0, 1);
    s.store_decision("h3", RetentionDecision::Evict, 0, 1);
    let rows = s.decision_snapshot();
    assert_eq!(rows[0].block_ref, "h1", "snapshot is sorted by block_ref");
    assert_eq!(rows[1].block_ref, "h2");
    assert_eq!(rows[2].decision, "evict");

    let restored = CachedPrefixState::new();
    restored.replace_decisions(&rows);
    assert_eq!(restored.decision_snapshot(), rows);
    assert!(
        restored.drain_if_dirty().is_none(),
        "a restore is already durable"
    );
}

#[test]
fn test_restore_skips_unknown_label() {
    let s = CachedPrefixState::new();
    s.replace_decisions(&[CachedBlockDecision {
        block_ref: "h1".into(),
        decision: "future-tier".into(),
    }]);
    assert!(s.decision_snapshot().is_empty());
}

#[test]
fn test_clear_decisions_drops_map() {
    let s = CachedPrefixState::new();
    s.store_decision("h1", RetentionDecision::Materialize, 0, 1);
    assert!(s.drain_if_dirty().is_some());
    s.clear_decisions();
    assert!(s.decision_snapshot().is_empty());
    assert!(s.drain_if_dirty().is_none());
}

/// A measurement reads the held decisions and records nothing. It must not
/// consume a pending recompute either: the turn that follows still pays it.
#[test]
fn test_measurement_reads_without_mutating() {
    let s = Arc::new(CachedPrefixState::new());
    s.record_turn(1_000, 800);
    let policy = CacheLivenessRetentionPolicy::new(Arc::clone(&s));
    assert_eq!(
        policy.decide(&ctx(0, Some("h1"), 1)),
        RetentionDecision::Materialize
    );
    assert!(
        s.drain_if_dirty().is_some(),
        "the turn recorded the first decision"
    );

    // A partial collapse leaves the prefix live but arms a full recompute.
    s.record_turn(2_000, 300);
    assert_eq!(
        policy.decide(&measure(8, Some("h1"), 2)),
        RetentionDecision::Evict,
        "the pane reflects the pending full recompute"
    );
    assert!(
        s.drain_if_dirty().is_none(),
        "a measurement records nothing"
    );
    // Had the measurement consumed the pending recompute, this turn would
    // see a live band and hold Materialize.
    assert_eq!(
        policy.decide(&ctx(8, Some("h1"), 2)),
        RetentionDecision::Evict,
        "the pending recompute is still there for the turn"
    );
}

/// The batch threshold counts only blocks the current assembly saw. Rows
/// that left the window keep an old age, and counting them would arm a
/// demotion for a window that no longer holds the batch.
#[test]
fn test_batch_counts_window_only() {
    let s = Arc::new(CachedPrefixState::new());
    s.record_turn(1_000, 800);
    for hash in ["h1", "h2", "h3", "h4"] {
        s.store_decision(hash, RetentionDecision::Materialize, 7, 1);
    }
    let policy = CacheLivenessRetentionPolicy::new(Arc::clone(&s));
    // Assembly 2 sees only h1; the other three left the window.
    assert_eq!(
        policy.decide(&ctx(7, Some("h1"), 2)),
        RetentionDecision::Materialize
    );
    // Assembly 3 still sees only h1: the departed rows did not arm a batch.
    assert_eq!(
        policy.decide(&ctx(7, Some("h1"), 3)),
        RetentionDecision::Materialize
    );
}

/// A row unseen for the prune window is dropped, so a long session's map
/// stays bounded to the turns that are still in the prefix.
#[test]
fn test_prune_drops_unseen() {
    let s = Arc::new(CachedPrefixState::new());
    s.record_turn(1_000, 800);
    s.store_decision("h1", RetentionDecision::Materialize, 0, 1);
    s.store_decision("h2", RetentionDecision::Materialize, 0, 1);
    let policy = CacheLivenessRetentionPolicy::new(Arc::clone(&s));
    // A turn with no block_refs still starts the assembly and prunes rows
    // last seen two turn assemblies ago.
    let _ = policy.decide(&ctx(0, None, 4));
    assert!(
        s.decision_snapshot().is_empty(),
        "rows unseen for the prune window are dropped"
    );
}

/// The durable payload carries only the blocks the current assembly saw.
#[test]
fn test_drain_keeps_current() {
    let s = CachedPrefixState::new();
    s.store_decision("h1", RetentionDecision::Materialize, 0, 1);
    s.store_decision("h2", RetentionDecision::Materialize, 0, 1);
    let _ = s.begin_assembly(2, 6);
    s.store_decision("h1", RetentionDecision::Summarize, 7, 2);
    let rows = s.drain_if_dirty().expect("the changed row is dirty");
    assert_eq!(rows.len(), 1, "the departed row is not in the payload");
    assert_eq!(rows[0].block_ref, "h1");
    assert_eq!(rows[0].decision, "summarize");
}

/// A batch demotion changes the prefix itself, so the resulting partial
/// cache read must not be mistaken for a server collapse and demote again.
#[test]
fn test_batch_no_rebreak() {
    let s = Arc::new(CachedPrefixState::new());
    s.record_turn(1_000, 100_000);
    let policy = CacheLivenessRetentionPolicy::new(Arc::clone(&s));
    for hash in ["b1", "b2", "b3", "b4", "young"] {
        assert_eq!(
            policy.decide(&ctx(0, Some(hash), 1)),
            RetentionDecision::Materialize
        );
    }
    // The four old blocks cross the batch threshold in this assembly.
    for hash in ["b1", "b2", "b3", "b4"] {
        assert_eq!(
            policy.decide(&ctx(7, Some(hash), 2)),
            RetentionDecision::Materialize
        );
    }
    // The next assembly demotes them once.
    for hash in ["b1", "b2", "b3", "b4"] {
        assert_eq!(
            policy.decide(&ctx(8, Some(hash), 3)),
            RetentionDecision::Summarize
        );
    }
    assert_eq!(
        policy.decide(&ctx(3, Some("young"), 3)),
        RetentionDecision::Materialize
    );
    assert!(
        s.retention_demoted(),
        "the batch demotion is attributed as retention"
    );
    // The server reports the expected partial read after our own change.
    s.record_turn(2_000, 20_000);
    // The following assembly must hold the new tiers, not demote again.
    for hash in ["b1", "b2", "b3", "b4"] {
        assert_eq!(
            policy.decide(&ctx(9, Some(hash), 4)),
            RetentionDecision::Summarize,
            "the batch's own drop is not a server collapse"
        );
    }
    assert_eq!(
        policy.decide(&ctx(4, Some("young"), 4)),
        RetentionDecision::Materialize,
        "the young block keeps its hold"
    );
}

/// One intentional prefix change exempts only the next response. A second
/// collapse in the same assembly (a continuation retry) must still arm the
/// full recompute.
#[test]
fn test_one_exemption_per_change() {
    let s = Arc::new(CachedPrefixState::new());
    s.record_turn(1_000, 100_000);
    let policy = CacheLivenessRetentionPolicy::new(Arc::clone(&s));
    for hash in ["b1", "b2", "b3", "b4", "young"] {
        assert_eq!(
            policy.decide(&ctx(0, Some(hash), 1)),
            RetentionDecision::Materialize
        );
    }
    for hash in ["b1", "b2", "b3", "b4"] {
        assert_eq!(
            policy.decide(&ctx(7, Some(hash), 2)),
            RetentionDecision::Materialize
        );
    }
    for hash in ["b1", "b2", "b3", "b4"] {
        assert_eq!(
            policy.decide(&ctx(8, Some(hash), 3)),
            RetentionDecision::Summarize
        );
    }
    // The first partial read is explained by the batch demotion.
    s.record_turn(2_000, 20_000);
    // The second partial read is a real collapse: the exemption is spent.
    s.record_turn(3_000, 9_000);
    assert_eq!(
        policy.decide(&ctx(4, Some("young"), 4)),
        RetentionDecision::Summarize,
        "the second collapse is not exempted by the first change"
    );
}
