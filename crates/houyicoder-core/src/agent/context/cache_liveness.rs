//! Cache-liveness signal + per-block stable retention decisions.
//!
//! Hold each block_ref's decision while the provider reports a cache read.
//! A reported collapse or a batch watermark ends the hold; a local clock does
//! not. Compaction and a model switch bump a generation and clear the map.

use std::sync::Arc;

use houyicoder_protocol::llm::Usage;

use crate::agent::Runner;
use crate::agent::context::retention::{
    BudgetedRetentionPolicy, RetentionContext, RetentionDecision, RetentionPolicy,
};

mod state;

#[cfg(test)]
pub(crate) use state::BATCH_DEMOTE_MIN_BYTES;
pub(crate) use state::CachedPrefixState;

use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// The provider prompt-cache TTL, in milliseconds. A break with an unchanged
/// prefix and an idle gap at least this long is attributed to the server
/// letting its cache expire. The retention hold itself does not expire on
/// this clock: only a reported miss ends it.
pub const CACHE_TTL_MS: u64 = 60 * 60 * 1000;

/// Cache-liveness policy wiring and per-turn cache-read state.
impl Runner {
    /// Install the cache-liveness retention policy on the context builder,
    /// sharing this runner's cached-prefix state. Called once at construction
    /// so the serve path's block_ref decisions stay stable while the cached
    /// prefix is live and recompute aggressively once it expires.
    pub(crate) fn wire_cache_liveness_policy(&self) {
        let cache_policy = Arc::new(CacheLivenessRetentionPolicy::new(Arc::clone(
            &self.cached_prefix,
        )));
        let budgeted = Arc::new(BudgetedRetentionPolicy::new(
            cache_policy,
            self.context_builder.tokenizer_clone(),
        ));
        self.context_builder.set_retention_policy(budgeted);
    }

    /// Stamp the completed turn's clock and reported cache read. A collapse
    /// marks every held decision for an age-based recompute; a hit after a
    /// collapse cancels it. An unreported reading advances only the clock.
    pub(crate) fn record_turn_cache(&self, usage: &Usage) {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        if usage.provenance.cache_read_reported {
            self.cached_prefix
                .record_turn(now_ms, usage.cache_read_input_tokens as u64);
            self.cached_prefix.clear_retention_demoted();
        } else {
            self.cached_prefix.touch_timestamp(now_ms);
        }
    }

    /// A shared handle to the cached-prefix state. The serve path's retention
    /// policy reads it; compact bumps the generation to clear per-block
    /// decisions.
    pub fn cached_prefix(&self) -> &Arc<CachedPrefixState> {
        &self.cached_prefix
    }
}

/// A retention policy that holds per-block decisions stable while the server
/// reports a cache read, and shifts the age thresholds with liveness: while
/// the cache is live, a wider recent band materializes so the prefix stays
/// intact; after a reported miss, the age-based 3-tier applies with no
/// stability constraint. Supersession still evicts regardless: a stale result
/// is never worth serving, and the surrounding content changes once the tool
/// runs again.
pub struct CacheLivenessRetentionPolicy {
    cached: Arc<CachedPrefixState>,
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

/// The age band's decision for one block.
fn tier(age_in_turns: u32, materialize_under: u32, summarize_under: u32) -> RetentionDecision {
    if age_in_turns < materialize_under {
        RetentionDecision::Materialize
    } else if age_in_turns < summarize_under {
        RetentionDecision::Summarize
    } else {
        RetentionDecision::Evict
    }
}

impl RetentionPolicy for CacheLivenessRetentionPolicy {
    fn decide(&self, ctx: &RetentionContext) -> RetentionDecision {
        // Start the assembly before the superseded shortcut so an all-Evict
        // window still consumes a pending recompute and prunes the map. A
        // measurement reads without consuming or recording.
        let alive = if ctx.mutating {
            self.cached
                .begin_assembly(ctx.assembly_id, self.materialize_turns_live)
        } else {
            self.cached.cache_live_readonly()
        };
        if ctx.is_superseded {
            if ctx.mutating
                && let Some(hash) = ctx.block_ref
            {
                self.cached.forget(hash);
            }
            return RetentionDecision::Evict;
        }
        let held = if ctx.mutating {
            ctx.block_ref.and_then(|hash| {
                self.cached.lookup_held(
                    hash,
                    ctx.age_in_turns,
                    self.materialize_turns_live,
                    ctx.assembly_id,
                )
            })
        } else {
            ctx.block_ref
                .and_then(|hash| self.cached.lookup_held_readonly(hash))
        };
        if alive && let Some(d) = held {
            return d;
        }
        let (materialize_under, summarize_under) = self.band(alive);
        let decision = tier(ctx.age_in_turns, materialize_under, summarize_under);
        if ctx.mutating
            && let Some(hash) = ctx.block_ref
        {
            self.cached
                .store_decision(hash, decision, ctx.age_in_turns, ctx.assembly_id);
        }
        decision
    }

    fn record_materialized_bytes(&self, block_ref: &str, bytes: usize) {
        self.cached.record_materialized_bytes(block_ref, bytes);
    }
}

#[cfg(test)]
mod tests;
