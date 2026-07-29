//! Cache-liveness signal + per-block stable retention decisions.
//!
//! A turn that hit the prompt cache and is within the cache TTL leaves a
//! cached prefix in the served view. Demoting a block_ref inside that prefix
//! (Materialize → Summarize/Evict) changes the prefix bytes, so the next
//! turn's cache read misses — a small per-block saving that breaks the whole
//! prefix. While the cache is live, retention must hold each block_ref's
//! decision stable across turns. A turn that missed the cache, or one past
//! the TTL, has no cached prefix to break, so the age-based 3-tier applies
//! with no stability constraint.
//!
//! The state is interior-mutable so the serve path (which holds no Runner
//! lock) can read + update it. The last successful API turn stamps the wall
//! clock + the observed cache-read tokens; compaction bumps a generation
//! counter + clears the per-block decision map (a compact rewrites the
//! provider-facing transcript, so the prior prefix is no longer a cache
//! baseline). Between compactions, a block_ref served one way in one turn is
//! served the same way the next turn while the cache is live — the per-block
//! map holds the decision so the prefix bytes do not change.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use houyicoder_protocol::llm::Usage;

use super::Runner;
use super::retention::{RetentionContext, RetentionDecision, RetentionPolicy};

/// The cache-liveness Runner surface: construction wiring + the per-turn
/// cache-read stamp. Lives here (not on the Runner's main impl) so the
/// Runner module stays under the file-size gate.
impl Runner {
    /// Install the cache-liveness retention policy on the context builder,
    /// sharing this runner's cached-prefix state. Called once at construction
    /// so the serve path's block_ref decisions stay stable while the cached
    /// prefix is live and recompute aggressively once it expires.
    pub(super) fn wire_cache_liveness_policy(&self) {
        self.context_builder
            .set_retention_policy(std::sync::Arc::new(CacheLivenessRetentionPolicy::new(
                std::sync::Arc::clone(&self.cached_prefix),
            )));
    }

    /// Stamp the cached-prefix state with a completed API turn's wall clock
    /// and observed cache-read tokens. The cache is live while these stay
    /// fresh, so the serve path's retention policy holds per-block decisions
    /// stable.
    pub(crate) fn record_turn_cache(&self, usage: &Usage) {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.cached_prefix
            .record_turn(now_ms, usage.cache_read_input_tokens as u64);
    }

    /// A shared handle to the cached-prefix state. The serve path's retention
    /// policy reads it; compact bumps the generation to clear per-block
    /// decisions.
    pub fn cached_prefix(&self) -> &std::sync::Arc<CachedPrefixState> {
        &self.cached_prefix
    }
}

/// The reusable prompt-cache prefix bound, in milliseconds. A turn whose last
/// cache read hit and whose completion is within this window is live —
/// demoting a block_ref inside the cached prefix would break it. Past it the
/// cache is dead and demotion is free. The bound is the server prompt-cache
/// TTL (1 hour for the 1h ephemeral breakpoint tier).
pub const CACHE_TTL_MS: u64 = 60 * 60 * 1000;

/// Per-session cached-prefix tracking: the generation watermark (bumped by
/// compaction), the last successful API turn's timestamp + cache-read tokens,
/// and a per-block_ref retention-decision map stable within a generation.
/// Interior-mutable so the serve path reads/writes without a Runner lock.
pub struct CachedPrefixState {
    generation: AtomicU64,
    last_completion_ts: AtomicU64,
    last_cache_read_tokens: AtomicU64,
    /// (generation when decided, decision) keyed by block_ref hash. A decision
    /// is stable within its generation; a generation bump (compaction) clears
    /// the map so the next serve recomputes against the new prefix.
    block_decisions: Mutex<HashMap<String, (u64, RetentionDecision)>>,
}

impl CachedPrefixState {
    pub fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
            last_completion_ts: AtomicU64::new(0),
            last_cache_read_tokens: AtomicU64::new(0),
            block_decisions: Mutex::new(HashMap::new()),
        }
    }

    /// Current cache generation. Bumped only by compaction.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Record a successful API turn: the wall-clock completion + the cache-read
    /// tokens the provider reported. The cache is live while these stay fresh.
    pub fn record_turn(&self, now_ms: u64, cache_read_tokens: u64) {
        self.last_completion_ts.store(now_ms, Ordering::Relaxed);
        self.last_cache_read_tokens
            .store(cache_read_tokens, Ordering::Relaxed);
    }

    /// The last turn's observed cache-read tokens (0 before the first turn or
    /// after a compaction clears the baseline).
    pub fn last_cache_read_tokens(&self) -> u64 {
        self.last_cache_read_tokens.load(Ordering::Relaxed)
    }

    /// Whether the cached prefix is live: the last turn hit the cache (nonzero
    /// cache-read) AND the completion is within the TTL window. A live prefix
    /// must not be mutated by retention demotion (it would break the cache for
    /// a small per-block saving); a dead one is free to demote aggressively.
    pub fn cache_alive_at(&self, now_ms: u64, ttl_ms: u64) -> bool {
        let cache_read = self.last_cache_read_tokens.load(Ordering::Relaxed);
        if cache_read == 0 {
            return false;
        }
        let last = self.last_completion_ts.load(Ordering::Relaxed);
        now_ms.saturating_sub(last) < ttl_ms
    }

    /// Compaction replaced the provider-facing transcript: bump the
    /// generation and clear the per-block decision map so the next serve
    /// recomputes against the new prefix. The prior prefix is no longer a
    /// valid cache baseline.
    pub fn invalidate(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut m) = self.block_decisions.lock() {
            m.clear();
        }
        self.last_cache_read_tokens.store(0, Ordering::Relaxed);
    }

    /// The last decision stored for this block_ref in the current generation,
    /// if any. None means a new block, or a generation just bumped — the caller
    /// computes a fresh decision + stores it.
    pub fn lookup_decision(&self, hash: &str) -> Option<RetentionDecision> {
        let m = self.block_decisions.lock().ok()?;
        let (stored_gen, dec) = m.get(hash)?;
        if *stored_gen == self.generation() {
            Some(*dec)
        } else {
            None
        }
    }

    /// Store a block_ref's decision stamped with the current generation.
    pub fn store_decision(&self, hash: &str, decision: RetentionDecision) {
        if let Ok(mut m) = self.block_decisions.lock() {
            m.insert(hash.to_string(), (self.generation(), decision));
        }
    }
}

impl Default for CachedPrefixState {
    fn default() -> Self {
        Self::new()
    }
}

/// A retention policy that holds per-block decisions stable within a cache
/// generation (so the cached prefix does not change as a block ages) and
/// shifts the age thresholds with cache liveness: while the cache is live,
/// a wider recent band materializes so the prefix stays intact; past the TTL
/// or after a miss, the age-based 3-tier applies with no stability constraint.
/// Supersession still evicts regardless — a stale result is never worth
/// serving, and the surrounding content changes once the model re-invokes.
pub struct CacheLivenessRetentionPolicy {
    cached: Arc<CachedPrefixState>,
    ttl_ms: u64,
    /// Conservative band (cache live): materialize under this many turns.
    pub materialize_turns_live: u32,
    /// Conservative band: summarize under this many turns; older evicts.
    pub summarize_turns_live: u32,
    /// Aggressive band (cache dead): materialize under this many turns.
    pub materialize_turns_dead: u32,
    /// Aggressive band: summarize under this many turns; older evicts.
    pub summarize_turns_dead: u32,
}

impl CacheLivenessRetentionPolicy {
    pub fn new(cached: Arc<CachedPrefixState>) -> Self {
        Self {
            cached,
            ttl_ms: CACHE_TTL_MS,
            materialize_turns_live: 6,
            summarize_turns_live: 12,
            materialize_turns_dead: 2,
            summarize_turns_dead: 6,
        }
    }

    fn band(&self, alive: bool) -> (u32, u32) {
        if alive {
            (self.materialize_turns_live, self.summarize_turns_live)
        } else {
            (self.materialize_turns_dead, self.summarize_turns_dead)
        }
    }
}

impl RetentionPolicy for CacheLivenessRetentionPolicy {
    fn decide(&self, ctx: &RetentionContext) -> RetentionDecision {
        if ctx.is_superseded {
            return RetentionDecision::Evict;
        }
        // When the cache is cold (provider TTL expired), use the aggressive
        // band (same as cache-dead) — the prefix is being rewritten anyway.
        let alive = if ctx.cache_cold {
            false
        } else {
            self.cached.cache_alive_at(ctx.now_ms, self.ttl_ms)
        };
        // A block_ref's decision is stable within a generation while the cache
        // is live — returning the stored decision keeps the prefix bytes
        // identical across turns so the cache does not thrash.
        if alive
            && let Some(hash) = ctx.block_ref
            && let Some(d) = self.cached.lookup_decision(hash)
        {
            return d;
        }
        let (mat, sum) = self.band(alive);
        let d = if ctx.age_in_turns < mat {
            RetentionDecision::Materialize
        } else if ctx.age_in_turns < sum {
            RetentionDecision::Summarize
        } else {
            RetentionDecision::Evict
        };
        if let Some(hash) = ctx.block_ref {
            self.cached.store_decision(hash, d);
        }
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(age: u32, block_ref: Option<&'a str>, now_ms: u64) -> RetentionContext<'a> {
        RetentionContext {
            age_in_turns: age,
            is_superseded: false,
            block_ref,
            now_ms,
            cache_cold: false,
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
    fn test_expired_cache_recomputes_aggressively() {
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
    fn test_superseded_evicts_regardless_liveness() {
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
}
