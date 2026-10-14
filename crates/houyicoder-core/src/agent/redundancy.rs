//! Redundant tool-call detection — a harness self-evolution observer.
//!
//! The model sometimes re-issues a tool call whose input matches a prior call
//! with no intervening write to the affected path: a context-loss re-read
//! (compaction dropped the prior result), a cognitive loop, or — strongest
//! signal — two identical calls in one assistant message before either
//! executes. These are reward targets for self-evolution (a prompt/skill
//! nudge), not security gates: the tracker records + logs; a future
//! /trajectory pane surfaces the pattern, a future PreToolUse Feedback can
//! steer the model.
//!
//! Wiring is independent of the user-configurable hook registry: the hook
//! fire points (run_pre_tool_use_gate, fire_post_tool_use) early-return when
//! no hooks are registered, so a tracker hung off them would silently never
//! run in most sessions. Instead the runner calls check_batch / record
//! directly at the same lifecycle points (resolve_turn before the gate;
//! execute_partitioned next to fire_post_tool_use). See the plan's
//! "intentional choice" note: check runs BEFORE the gate, so Deny/Feedback/
//! Ask-removed calls are still checked — the model DID emit a duplicate;
//! the block is downstream and does not erase the cognitive signal.
//!
//! Identity: the tracker assigns a monotonic seq per EXECUTED call (record),
//! not per turn (turn is not threaded into the tool path, and replaying the
//! session log to count a turn is O(n)). "the Nth tool call since the last same-input call" is
//! more actionable than turn distance anyway. check_batch reads self.seq
//! (no mutation); record increments it.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};

use serde_json::Value;

use crate::observability::evolution::{
    REDUNDANCY_INPUT_PREVIEW_CAP, REDUNDANCY_RECORDS_CAP, RedundancyKind, RedundantCall,
};
use crate::observability::truncate_str;
#[cfg(test)]
use std::iter::once;

/// Cap on the ledger so a very long session does not grow it without bound.
/// LRU-evict (drop a random-ish entry) when exceeded — the ledger is a
/// best-effort recent-calls index, not a complete history. 512 = enough for
/// a typical session's dedup window (most sessions < 500 tool calls);
/// lower = more false negatives on cross-batch redundancy (a call 300 calls
/// ago evicted before a repeat is caught); higher = more memory per process.
const LEDGER_CAP: usize = 512;

/// Tools whose execution changes file/memory state, so a later same-input
/// call after one of these is NOT redundant (the state changed). Tracked via
/// the global last-write seq — coarse (a write to file X exempts a re-read
/// of file Y) but false-negative-safe: it can only miss a redundant flag,
/// never raise a false one. Per-path precision is a later refinement.
const WRITE_TOOLS: &[&str] = &["write", "edit", "multiedit", "save_memory", "delete_memory"];

/// The outcome of a prior same-input call, recorded in the ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LastOutcome {
    Success,
    Error,
}

/// One ledger entry: the seq + outcome of the last call with this key.
#[derive(Debug, Clone, Copy)]
struct CallRecord {
    last_seq: u64,
    outcome: LastOutcome,
}

/// In-memory redundant-call detector. Held by the Runner behind a std Mutex
/// (the runner is &self everywhere; lock is held only for brief pure compute
/// + Vec push, never across an await — matches cancel / queued_input).
#[derive(Debug)]
pub struct RedundancyTracker {
    /// Monotonic counter, incremented once per EXECUTED call (record).
    seq: u64,
    /// Monotonic counter of flagged redundant calls (never resets, never
    /// caps). Used by observe_redundancy to compute "how many new flags
    /// landed in this batch" — the ring length saturates at the cap, so
    /// using records.len() as a delta base silently breaks after 256.
    flagged_total: u64,
    /// key -> last call's (seq, outcome). Best-effort recent-calls index.
    ledger: HashMap<u64, CallRecord>,
    /// Seq of the last write-tool call. A call whose last_seq >= this had no
    /// write after it -> redundant; last_seq < this -> state changed -> not.
    last_global_write_seq: u64,
    /// Capped ring of flagged redundant calls for the /trajectory pane.
    records: VecDeque<RedundantCall>,
    /// Count of blind retries: a same-input call re-issued after the prior
    /// same-input call failed, with no intervening write. This is the
    /// reward signal (agent decision: retrying a known-failed call without
    /// changing anything), distinct from fail_count (world state: a command
    /// failed). The dream gate keys on this, not fail_count.
    retry_after_error: u32,
    /// Tool names of recent blind retries, capped like the records ring.
    /// Used by observe_redundancy to append a real-time MetaUser warning
    /// so the agent course-corrects within the current query, not just in
    /// the next query after the dream writes a lesson.
    retry_tools: VecDeque<String>,
}

impl Default for RedundancyTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl RedundancyTracker {
    pub fn new() -> Self {
        Self {
            seq: 0,
            flagged_total: 0,
            ledger: HashMap::new(),
            last_global_write_seq: 0,
            records: VecDeque::new(),
            retry_after_error: 0,
            retry_tools: VecDeque::new(),
        }
    }

    /// Count of blind retries (same-input call after a prior same-input
    /// failure, no intervening write). The reward-dream gate keys on this.
    pub fn retry_after_error(&self) -> u32 {
        self.retry_after_error
    }

    /// Hash a (tool_name, input) pair into a key. input.to_string() is
    /// canonical: the workspace enables serde_json preserve_order nowhere
    /// (see the protocol layer's tool-invocation doc — to_string emits
    /// key-sorted JSON, deterministic regardless of insertion order), so the
    /// same input always hashes the same. std DefaultHasher suffices (not a
    /// hot path, no crypto need, no new dependency).
    fn key_for(tool_name: &str, input: &Value) -> u64 {
        let mut h = DefaultHasher::new();
        tool_name.hash(&mut h);
        input.to_string().hash(&mut h);
        h.finish()
    }

    /// Check a batch for redundant calls BEFORE execution. Carries a
    /// batch-local seen set so two identical calls in one assistant message
    /// (the strongest signal) are flagged — the ledger alone can't catch
    /// them since neither has executed yet. Does NOT mutate the ledger
    /// (record() does, after execution). Non-blocking: records + logs only.
    pub fn check_batch<'a, I>(&mut self, calls: I)
    where
        I: IntoIterator<Item = (&'a str, &'a Value)>,
    {
        let mut seen: HashSet<u64> = HashSet::new();
        for (tool_name, input) in calls {
            let key = Self::key_for(tool_name, input);
            // Same-batch self-repeat: the strongest signal. Even "context
            // loss" can't explain emitting the same call twice in one
            // message — the model has not seen any result yet.
            if seen.contains(&key) {
                self.flag(RedundancyKind::SameBatch, 0, tool_name, input, None);
            } else if let Some(rec) = self.ledger.get(&key) {
                // A prior same-input call exists. Only act when no write has
                // landed since (last_global_write_seq <= rec.last_seq); an
                // intervening write changes state, so a re-call is fresh.
                if self.last_global_write_seq <= rec.last_seq {
                    if rec.outcome == LastOutcome::Success {
                        // Re-issuing a call whose prior result is still valid
                        // is the redundant smell — context-loss re-read or a
                        // cognitive loop. Flag it.
                        let gap = self.seq.saturating_sub(rec.last_seq);
                        self.flag(
                            RedundancyKind::CrossBatch,
                            gap,
                            tool_name,
                            input,
                            Some(rec.last_seq),
                        );
                    } else {
                        // Re-issuing a call that failed last time, still with
                        // no intervening write, is a blind retry. It is not
                        // redundant (the prior failed) but it is the reward
                        // signal reward-dream mines: an agent decision to
                        // retry a known-failed call without changing anything.
                        self.retry_after_error += 1;
                        if self.retry_tools.len() >= REDUNDANCY_RECORDS_CAP {
                            self.retry_tools.pop_front();
                        }
                        self.retry_tools.push_back(tool_name.to_string());
                    }
                }
            }
            seen.insert(key);
        }
    }

    /// Record one EXECUTED call's outcome into the ledger + bump the write
    /// seq if it was a write-tool. Called after the tool returns (next to
    /// fire_post_tool_use). is_error=true (including the Interrupted
    /// fallback result) records as Error, so a
    /// later same-input retry is not flagged — Esc-then-retry is legit.
    pub fn record(&mut self, tool_name: &str, input: &Value, is_error: bool) {
        self.seq += 1;
        let key = Self::key_for(tool_name, input);
        let outcome = if is_error {
            LastOutcome::Error
        } else {
            LastOutcome::Success
        };
        self.ledger.insert(
            key,
            CallRecord {
                last_seq: self.seq,
                outcome,
            },
        );
        if WRITE_TOOLS.contains(&tool_name) {
            self.last_global_write_seq = self.seq;
        }
        // LRU-ish evict: keep the ledger bounded for very long sessions.
        if self.ledger.len() > LEDGER_CAP {
            // Evict the entry with the smallest last_seq (oldest). O(n) but
            // rare (only on overflow) and n is bounded by LEDGER_CAP.
            if let Some(oldest_key) = self
                .ledger
                .iter()
                .min_by_key(|(_, rec)| rec.last_seq)
                .map(|(k, _)| *k)
            {
                self.ledger.remove(&oldest_key);
            }
        }
    }

    /// Build a RedundantCall, cap its preview, push to the ring (drop oldest
    /// on overflow), and emit a tracing line. The durable sink is the log;
    /// the ring is a live buffer for the future /trajectory pane.
    fn flag(
        &mut self,
        kind: RedundancyKind,
        gap: u64,
        tool_name: &str,
        input: &Value,
        last_seq: Option<u64>,
    ) {
        let preview = truncate_str(&input.to_string(), REDUNDANCY_INPUT_PREVIEW_CAP);
        tracing::debug!(tool = tool_name, ?kind, gap, "redundant call flagged");
        let rec = RedundantCall {
            tool: tool_name.to_string(),
            input_preview: preview,
            kind,
            gap,
            last_seq: last_seq.unwrap_or(0),
            prior_ref: None,
        };
        if self.records.len() >= REDUNDANCY_RECORDS_CAP {
            self.records.pop_front();
        }
        self.records.push_back(rec);
        self.flagged_total = self.flagged_total.saturating_add(1);
    }

    /// Monotonic count of all flagged redundant calls (never resets,
    /// never caps). Callers use the delta between two reads to compute
    /// "how many new flags landed in this batch" — the ring length
    /// saturates at the cap, so using records().len() as a delta base
    /// silently breaks after the ring fills.
    pub fn flagged_total(&self) -> u64 {
        self.flagged_total
    }

    /// Snapshot of flagged redundant calls (for the /trajectory pane). Oldest
    /// first. The pane's own sprint consumes this.
    pub fn records(&self) -> &VecDeque<RedundantCall> {
        &self.records
    }

    /// Snapshot of tool names from recent blind retries. Used by
    /// observe_redundancy to append a real-time MetaUser warning per
    /// blind-retry tool so the agent course-corrects within the query.
    pub fn retry_tools(&self) -> &VecDeque<String> {
        &self.retry_tools
    }
}

#[cfg(test)]
mod tests;
