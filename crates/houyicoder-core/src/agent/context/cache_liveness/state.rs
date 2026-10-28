//! The per-block decision state machine.
//!
//! The map holds one decision per block_ref, the generation it was decided
//! in, how old the block was when it was last served, how many bytes its
//! materialized form held, and which turn assembly last saw it. The last
//! fact exists for the batch exit: counting blocks that left the window
//! would arm a demotion on every assembly instead of only when the held set
//! is actually large. A superseded block is removed by forget rather than
//! kept at its old tier.
//!
//! One lock covers the map and the assembly-scoped flags, so a lookup that
//! updates an age and a record that marks a recompute cannot interleave into
//! a state neither writer intended.

use std::collections::HashMap;
use std::mem::{replace, take};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use houyicoder_context::CachedBlockDecision;

use crate::agent::context::retention::RetentionDecision;

/// How many over-age materialized blocks trigger one batch demotion.
const BATCH_DEMOTE_MIN_BLOCKS: usize = 4;

/// How many bytes the over-age materialized blocks must hold to trigger one
/// batch demotion. A single large block can cross this on its own.
pub(crate) const BATCH_DEMOTE_MIN_BYTES: u64 = 64 * 1024;

/// How many turn assemblies a decision may go unseen before it is pruned.
/// Every block_ref in the window is decided on every turn, so an entry past
/// this floor is not in the prefix; recomputing it when it returns is free.
const PRUNE_AFTER_ASSEMBLIES: u64 = 2;

/// One block_ref's frozen decision plus the facts the batch threshold reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HeldDecision {
    generation: u64,
    decision: RetentionDecision,
    last_age: u32,
    bytes: Option<u64>,
    last_seen_assembly: u64,
}

/// What the next assembly must recompute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Recompute {
    /// Keep the held decisions.
    None,
    /// The server cache collapsed: recompute every block under the dead band.
    All,
    /// The held set crossed the batch threshold: demote the over-age
    /// materialized blocks and keep the younger ones frozen.
    OverAge,
}

/// The decision map and the assembly-scoped flags that read it.
#[derive(Debug)]
struct CacheState {
    decisions: HashMap<String, HeldDecision>,
    /// A recompute armed by a server miss or by the batch threshold. The
    /// next turn assembly consumes it; a measurement never does.
    pending: Recompute,
    /// The current generation's band is the aggressive one, set while a
    /// pending recompute is being paid for.
    dead_band: bool,
    /// Whether the prefix is live for the assembly in progress.
    alive: bool,
    /// The last turn assembly that ran, not a measurement.
    last_assembly: Option<u64>,
    /// A decision write changed the durable map since the last record.
    dirty: bool,
    /// This assembly intentionally changed the prefix bytes. A reported drop
    /// after it is the expected cost of that change, not a server miss.
    prefix_changed: bool,
    /// The change above was a retention demotion, not a supersession or a
    /// block re-entering the window. Reported as its own break cause.
    retention_demoted: bool,
}

impl CacheState {
    fn new() -> Self {
        Self {
            decisions: HashMap::new(),
            pending: Recompute::None,
            dead_band: false,
            alive: false,
            last_assembly: None,
            dirty: false,
            prefix_changed: false,
            retention_demoted: false,
        }
    }
}

/// Per-session cached-prefix tracking: the generation watermark (bumped by
/// compaction and a model switch), the last successful API turn's timestamp
/// and cache-read tokens, and the per-block decisions held within a
/// generation. Interior-mutable so the serve path reads and writes without a
/// Runner lock.
pub struct CachedPrefixState {
    generation: AtomicU64,
    last_completion_ts: AtomicU64,
    last_cache_read_tokens: AtomicU64,
    state: Mutex<CacheState>,
}

impl CachedPrefixState {
    pub fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
            last_completion_ts: AtomicU64::new(0),
            last_cache_read_tokens: AtomicU64::new(0),
            state: Mutex::new(CacheState::new()),
        }
    }

    /// Current cache generation. Bumped by compaction and a model switch.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Record a successful API turn: the wall-clock completion plus the
    /// cache-read tokens the provider reported. A reported collapse against
    /// the previous reading marks every held decision for a recompute; a
    /// reported hit after a collapse cancels it because the server rebuilt
    /// the prefix from the decisions the previous assembly sent.
    pub fn record_turn(&self, now_ms: u64, cache_read_tokens: u64) {
        let previous = self
            .last_cache_read_tokens
            .swap(cache_read_tokens, Ordering::Relaxed);
        self.last_completion_ts.store(now_ms, Ordering::Relaxed);
        let collapsed =
            previous > 0 && (cache_read_tokens == 0 || cache_read_tokens < previous / 2);
        if let Ok(mut state) = self.state.lock() {
            // One intentional prefix change exempts the one response that
            // follows it. A continuation response in the same assembly must
            // not inherit the exemption from its predecessor.
            let prefix_changed = take(&mut state.prefix_changed);
            if collapsed && (cache_read_tokens == 0 || !prefix_changed) {
                state.pending = Recompute::All;
            } else if cache_read_tokens > 0 {
                state.dead_band = false;
                if state.pending == Recompute::All {
                    state.pending = Recompute::None;
                }
            }
        }
    }

    /// Seed the state from a durable turn when a session is reopened. The
    /// last reported cache read is the baseline for the next collapse, and
    /// the completion timestamp is the last activity the log holds. No
    /// collapse is inferred here: this is the starting point, not a
    /// transition.
    pub fn seed_turn(&self, now_ms: u64, cache_read_tokens: Option<u64>) {
        self.last_completion_ts.store(now_ms, Ordering::Relaxed);
        if let Some(tokens) = cache_read_tokens {
            self.last_cache_read_tokens.store(tokens, Ordering::Relaxed);
        }
        if let Ok(mut state) = self.state.lock() {
            state.pending = Recompute::None;
            state.dead_band = false;
        }
    }

    /// The wall clock of the last completed API turn, or 0 before one. A
    /// resumed session seeds it from the log so a first-turn break can still
    /// name an idle gap when no prefix hash has been recorded in this process.
    pub fn last_completion_ts(&self) -> u64 {
        self.last_completion_ts.load(Ordering::Relaxed)
    }

    /// The last turn's observed cache-read tokens (0 before the first turn or
    /// after a compaction clears the baseline).
    pub fn last_cache_read_tokens(&self) -> u64 {
        self.last_cache_read_tokens.load(Ordering::Relaxed)
    }

    /// Advance the completion timestamp without altering the cache-read count.
    /// Used when a provider did not report a cache-read field: the unknown
    /// zero must not overwrite a previously reported hit.
    pub fn touch_timestamp(&self, now_ms: u64) {
        self.last_completion_ts.store(now_ms, Ordering::Relaxed);
    }

    /// Whether the last turn reported a cache read. A live prefix must not be
    /// mutated by retention demotion; a dead one is free to demote
    /// aggressively. The answer is evidence from the provider, not a local
    /// clock: a prefix can outlive the declared breakpoint TTL.
    pub fn cache_live(&self) -> bool {
        self.last_cache_read_tokens.load(Ordering::Relaxed) > 0
    }

    /// Compaction or a model switch replaced the provider-facing transcript:
    /// bump the generation and clear the decisions so the next serve
    /// recomputes against the new prefix. The prior prefix is no longer a
    /// valid cache baseline.
    pub fn invalidate(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.last_cache_read_tokens.store(0, Ordering::Relaxed);
        if let Ok(mut state) = self.state.lock() {
            state.decisions.clear();
            state.pending = Recompute::None;
            state.dead_band = false;
            state.alive = false;
            state.dirty = false;
            state.prefix_changed = false;
            state.retention_demoted = false;
        }
    }

    /// Start one turn assembly and return whether the prefix is live for it.
    /// The first call of an assembly consumes any pending recompute; later
    /// calls in the same assembly reuse the answer so every block sees one
    /// band.
    pub(super) fn begin_assembly(&self, assembly_id: u64, materialize_turns_live: u32) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return self.cache_live();
        };
        if state.last_assembly != Some(assembly_id) {
            state.last_assembly = Some(assembly_id);
            state.prefix_changed = false;
            state.retention_demoted = false;
            let floor = assembly_id.saturating_sub(PRUNE_AFTER_ASSEMBLIES);
            let before = state.decisions.len();
            state.decisions.retain(|_, held| {
                held.generation == self.generation() && held.last_seen_assembly >= floor
            });
            if state.decisions.len() != before {
                state.dirty = true;
            }
            match replace(&mut state.pending, Recompute::None) {
                Recompute::All => {
                    state.prefix_changed = !state.decisions.is_empty();
                    state.retention_demoted = state.prefix_changed;
                    state.decisions.clear();
                    state.dead_band = true;
                }
                Recompute::OverAge => {
                    let generation = self.generation();
                    let before = state.decisions.len();
                    state.decisions.retain(|_, held| {
                        held.generation == generation
                            && (held.decision != RetentionDecision::Materialize
                                || held.last_age < materialize_turns_live)
                    });
                    state.prefix_changed = state.decisions.len() != before;
                    state.retention_demoted = state.prefix_changed;
                    state.dead_band = false;
                }
                Recompute::None => {}
            }
            state.alive = self.cache_live() && !state.dead_band;
        }
        state.alive
    }

    /// The live band for a measurement: read-only, so it consumes no pending
    /// recompute and writes no age or dirty bit. A pending full recompute is
    /// already the next turn's band; an over-age batch is not.
    pub(super) fn cache_live_readonly(&self) -> bool {
        let (dead, pending_all) = self
            .state
            .lock()
            .map(|state| (state.dead_band, state.pending == Recompute::All))
            .unwrap_or((false, false));
        self.cache_live() && !dead && !pending_all
    }

    /// The held decision for this block, when the cache is live. A held
    /// materialized block that has aged past the live band updates the batch
    /// count; once the held set in this assembly crosses the threshold, the
    /// next assembly demotes the over-age blocks together.
    pub(super) fn lookup_held(
        &self,
        hash: &str,
        age_in_turns: u32,
        materialize_turns_live: u32,
        assembly_id: u64,
    ) -> Option<RetentionDecision> {
        let generation = self.generation();
        let mut state = self.state.lock().ok()?;
        let (decision, over_age) = {
            let held = state.decisions.get_mut(hash)?;
            if held.generation != generation {
                return None;
            }
            held.last_age = age_in_turns;
            held.last_seen_assembly = assembly_id;
            (
                held.decision,
                held.decision == RetentionDecision::Materialize
                    && age_in_turns >= materialize_turns_live,
            )
        };
        if over_age && state.pending == Recompute::None {
            let mut blocks = 0usize;
            let mut bytes = 0u64;
            for held in state.decisions.values() {
                if held.generation == generation
                    && held.last_seen_assembly == assembly_id
                    && held.decision == RetentionDecision::Materialize
                    && held.last_age >= materialize_turns_live
                {
                    blocks += 1;
                    bytes = bytes.saturating_add(held.bytes.unwrap_or(0));
                }
            }
            if blocks >= BATCH_DEMOTE_MIN_BLOCKS || bytes >= BATCH_DEMOTE_MIN_BYTES {
                state.pending = Recompute::OverAge;
            }
        }
        Some(decision)
    }

    /// The held decision for a measurement. Read-only.
    pub(super) fn lookup_held_readonly(&self, hash: &str) -> Option<RetentionDecision> {
        let generation = self.generation();
        let state = self.state.lock().ok()?;
        let held = state.decisions.get(hash)?;
        (held.generation == generation).then_some(held.decision)
    }

    /// Store a block_ref's decision stamped with the current generation. A
    /// write that changes the decision marks the map dirty, so the durable
    /// log can record the new map once per turn instead of after a scan.
    pub(super) fn store_decision(
        &self,
        hash: &str,
        decision: RetentionDecision,
        age_in_turns: u32,
        assembly_id: u64,
    ) {
        let generation = self.generation();
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        match state.decisions.get(hash).copied() {
            Some(held) if held.generation == generation => {
                if held.decision == decision {
                    if let Some(entry) = state.decisions.get_mut(hash) {
                        entry.last_age = age_in_turns;
                        entry.last_seen_assembly = assembly_id;
                    }
                } else {
                    state.decisions.insert(
                        hash.to_string(),
                        HeldDecision {
                            generation,
                            decision,
                            last_age: age_in_turns,
                            bytes: held.bytes,
                            last_seen_assembly: assembly_id,
                        },
                    );
                    state.dirty = true;
                    state.prefix_changed = true;
                }
            }
            _ => {
                state.decisions.insert(
                    hash.to_string(),
                    HeldDecision {
                        generation,
                        decision,
                        last_age: age_in_turns,
                        bytes: None,
                        last_seen_assembly: assembly_id,
                    },
                );
                state.dirty = true;
                if age_in_turns > 0 {
                    state.prefix_changed = true;
                }
            }
        }
    }

    /// Drop a block's decision. A superseded result is served as Evict
    /// without freezing that tier, so keeping its old Materialize entry would
    /// both count toward the batch threshold and survive a resume as a
    /// decision the last request no longer sent.
    pub(super) fn forget(&self, hash: &str) {
        if let Ok(mut state) = self.state.lock()
            && state.decisions.remove(hash).is_some()
        {
            state.dirty = true;
            state.prefix_changed = true;
        }
    }

    /// Record how many bytes a materialized block held, so the batch
    /// threshold can measure the context the hold is paying for.
    pub(super) fn record_materialized_bytes(&self, hash: &str, bytes: usize) {
        if let Ok(mut state) = self.state.lock()
            && let Some(held) = state.decisions.get_mut(hash)
        {
            held.bytes = Some(bytes as u64);
        }
    }

    /// Whether the last turn assembly demoted a held decision. The break
    /// detector reads this once, before the cache state advances.
    pub fn retention_demoted(&self) -> bool {
        self.state
            .lock()
            .map(|state| state.retention_demoted)
            .unwrap_or(false)
    }

    /// Clear the demotion marker after the response that observed it.
    pub fn clear_retention_demoted(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.retention_demoted = false;
        }
    }

    /// Mark a retention demotion for the break-attribution test.
    #[cfg(test)]
    pub(crate) fn mark_retention_demoted(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.retention_demoted = true;
        }
    }

    /// Take the durable map when a decision write changed it. The payload
    /// carries only the blocks this turn assembly saw, so a long session's
    /// log does not accumulate rows for blocks that left the window. None
    /// means nothing changed and no record is needed.
    pub fn drain_if_dirty(&self) -> Option<Vec<CachedBlockDecision>> {
        let mut state = self.state.lock().ok()?;
        if !take(&mut state.dirty) {
            return None;
        }
        let generation = self.generation();
        let current = state.last_assembly.unwrap_or(0);
        let mut rows: Vec<CachedBlockDecision> = state
            .decisions
            .iter()
            .filter(|(_, held)| held.generation == generation && held.last_seen_assembly == current)
            .map(|(hash, held)| CachedBlockDecision {
                block_ref: hash.clone(),
                decision: held.decision.label().to_string(),
            })
            .collect();
        rows.sort_by(|a, b| a.block_ref.cmp(&b.block_ref));
        Some(rows)
    }

    /// The current decisions, sorted by block_ref. Read-only, for tests and
    /// diagnostics; the durable write uses drain_if_dirty.
    pub fn decision_snapshot(&self) -> Vec<CachedBlockDecision> {
        let generation = self.generation();
        let Ok(state) = self.state.lock() else {
            return Vec::new();
        };
        let mut rows: Vec<CachedBlockDecision> = state
            .decisions
            .iter()
            .filter(|(_, held)| held.generation == generation)
            .map(|(hash, held)| CachedBlockDecision {
                block_ref: hash.clone(),
                decision: held.decision.label().to_string(),
            })
            .collect();
        rows.sort_by(|a, b| a.block_ref.cmp(&b.block_ref));
        rows
    }

    /// Replace the decision map from a durable record. An unknown label is
    /// skipped rather than guessed, so a log written by a newer build cannot
    /// turn into a wrong retention tier.
    pub fn replace_decisions(&self, decisions: &[CachedBlockDecision]) {
        let generation = self.generation();
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.decisions.clear();
        for row in decisions {
            if let Some(decision) = RetentionDecision::from_label(&row.decision) {
                state.decisions.insert(
                    row.block_ref.clone(),
                    HeldDecision {
                        generation,
                        decision,
                        last_age: 0,
                        bytes: None,
                        last_seen_assembly: 0,
                    },
                );
            }
        }
        state.dirty = false;
    }

    /// Drop every held decision. A compaction, a clear, or a model switch
    /// replaces the prefix, so the old decisions no longer describe it.
    pub fn clear_decisions(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.decisions.clear();
            state.pending = Recompute::None;
            state.dead_band = false;
            state.alive = false;
            state.dirty = false;
            state.prefix_changed = false;
            state.retention_demoted = false;
        }
    }
}

impl Default for CachedPrefixState {
    fn default() -> Self {
        Self::new()
    }
}
