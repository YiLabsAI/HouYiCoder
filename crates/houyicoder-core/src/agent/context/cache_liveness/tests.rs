//! Tests for this module.

use super::*;

fn ctx<'a>(age: u32, block_ref: Option<&'a str>, now_ms: u64) -> RetentionContext<'a> {
    RetentionContext {
        age_in_turns: age,
        is_superseded: false,
        block_ref,
        now_ms,
    }
}

#[test]
fn test_alive_requires_hit_fresh() {
    let s = CachedPrefixState::new();
    // No turn recorded: not alive (no cache to break).
    assert!(!s.cache_alive_at(1_000, CACHE_TTL_MS));
    // A turn that hit the cache + is fresh: alive.
    s.record_turn(1_000, 800);
    assert!(s.cache_alive_at(2_000, CACHE_TTL_MS));
    // Past the TTL: dead.
    assert!(!s.cache_alive_at(1_000 + CACHE_TTL_MS + 1, CACHE_TTL_MS));
    // A turn that missed (zero cache read): dead.
    s.record_turn(3_000, 0);
    assert!(!s.cache_alive_at(3_001, CACHE_TTL_MS));
}

#[test]
fn test_invalidate_clears_decisions() {
    let s = CachedPrefixState::new();
    s.record_turn(1_000, 800);
    s.store_decision("h1", RetentionDecision::Materialize);
    assert_eq!(s.generation(), 0);
    assert_eq!(
        s.lookup_decision("h1"),
        Some(RetentionDecision::Materialize)
    );
    s.invalidate();
    assert_eq!(s.generation(), 1);
    assert!(
        s.lookup_decision("h1").is_none(),
        "map cleared on invalidate"
    );
    assert_eq!(s.last_cache_read_tokens(), 0, "baseline cleared");
    assert!(!s.cache_alive_at(1_001, CACHE_TTL_MS));
}

#[test]
fn test_decision_stable_while_alive() {
    // The same block_ref served twice while the cache is live returns the
    // same decision — the prefix bytes do not flap as the block ages.
    let s = Arc::new(CachedPrefixState::new());
    s.record_turn(1_000, 800); // alive
    let policy = CacheLivenessRetentionPolicy::new(Arc::clone(&s));
    // First serve at age 1 (alive, conservative band: age 1 < 6 → Materialize).
    let d1 = policy.decide(&ctx(1, Some("h1"), 1_100));
    assert_eq!(d1, RetentionDecision::Materialize);
    // Second serve at age 5 (would still Materialize under conservative,
    // but the stored decision holds regardless).
    let d2 = policy.decide(&ctx(5, Some("h1"), 1_200));
    assert_eq!(d2, d1, "decision stable within generation while alive");
    // A different block at the same age gets its own decision.
    let d3 = policy.decide(&ctx(1, Some("h2"), 1_300));
    assert_eq!(d3, RetentionDecision::Materialize);
}

#[test]
fn test_expired_cache_recomputes_policy() {
    // While the cache is live, a block_ref stores its decision. Once the
    // TTL elapses (or the last turn missed), the stored decision is no
    // longer consulted — the aggressive band applies, so an age-3 block
    // Summarizes (3 >= 2) instead of the conservative Materialize.
    let s = Arc::new(CachedPrefixState::new());
    // Record a live turn, then serve once to store a Materialize decision.
    s.record_turn(1_000, 800);
    let policy = CacheLivenessRetentionPolicy::new(Arc::clone(&s));
    let _ = policy.decide(&ctx(3, Some("h1"), 1_100));
    // The TTL elapses + the next turn missed (zero cache read).
    s.record_turn(1_000 + CACHE_TTL_MS + 5_000, 0);
    let d = policy.decide(&ctx(3, Some("h1"), 1_000 + CACHE_TTL_MS + 5_001));
    assert_eq!(
        d,
        RetentionDecision::Summarize,
        "past the TTL the aggressive band overrides the stored decision"
    );
}

#[test]
fn test_policy_evicts_superseded_block() {
    let s = Arc::new(CachedPrefixState::new());
    s.record_turn(1_000, 800);
    let policy = CacheLivenessRetentionPolicy::new(Arc::clone(&s));
    let mut c = ctx(0, Some("h1"), 1_100);
    c.is_superseded = true;
    assert_eq!(policy.decide(&c), RetentionDecision::Evict);
}

#[test]
fn test_live_uses_conservative_band() {
    // Alive: age 4 (>= aggressive materialize 2 but < conservative 6) →
    // Materialize under the conservative band, not Summarize.
    let s = Arc::new(CachedPrefixState::new());
    s.record_turn(1_000, 800);
    let policy = CacheLivenessRetentionPolicy::new(Arc::clone(&s));
    let d = policy.decide(&ctx(4, Some("h1"), 1_100));
    assert_eq!(d, RetentionDecision::Materialize);
}

/// When the provider did not report a cache-read count, touch_timestamp
/// advances the clock without zeroing a previously reported hit. The liveness
/// state is preserved — an unreported reading is unknown, not a confirmed
/// cache miss.
#[test]
fn test_touch_timestamp_preserves_liveness() {
    let s = CachedPrefixState::new();
    // A reported turn: cache hit at 800, clock at 1_000.
    s.record_turn(1_000, 800);
    assert_eq!(s.last_cache_read_tokens(), 800);
    assert!(s.cache_alive_at(1_001, CACHE_TTL_MS));
    // An unreported turn comes in — only the clock advances, cache_read
    // stays at the last reported value so the prefix stays live.
    s.touch_timestamp(2_000);
    assert_eq!(s.last_cache_read_tokens(), 800, "cache-read preserved");
    assert!(
        s.cache_alive_at(2_001, CACHE_TTL_MS),
        "still alive after touch: cache-read unchanged"
    );
    // A real miss (zero cache_read from record_turn) should still kill liveness.
    s.record_turn(2_100, 0);
    assert!(
        !s.cache_alive_at(2_101, CACHE_TTL_MS),
        "confirmed miss kills liveness"
    );
}
