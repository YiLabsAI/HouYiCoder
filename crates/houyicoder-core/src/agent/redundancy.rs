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
//! fire points (arbitrate_pre_tool_use, fire_post_tool_use) early-return when
//! no hooks are registered, so a tracker hung off them would silently never
//! run in most sessions. Instead the runner calls check_batch / record
//! directly at the same lifecycle points (resolve_turn before arbitrate;
//! execute_partitioned next to fire_post_tool_use). See the plan's
//! "intentional choice" note: check runs BEFORE arbitrate, so Deny/Feedback/
//! Ask-removed calls are still checked — the model DID emit a duplicate;
//! the block is downstream and does not erase the cognitive signal.
//!
//! Identity: the tracker mints a monotonic seq per EXECUTED call (record),
//! not per turn (turn is not threaded into the tool path and count_turns is
//! an O(n) replay). "the Nth tool call since the last same-input call" is
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
    /// fire_post_tool_use). is_error=true (including the synthetic
    /// Interrupted {"error":"interrupted by user"}) records as Error, so a
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
mod tests {
    use super::*;
    use serde_json::json;

    fn calls<'a>(
        spec: &'a [(&'a str, serde_json::Value)],
    ) -> Vec<(&'a str, &'a serde_json::Value)> {
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

    /// Esc-interrupt then retry: the synthetic Interrupted result carries
    /// {"error":"interrupted by user"} so record sees is_error=true →
    /// outcome=Error → the retry is not flagged. Pins the tracker side of the
    /// dependency; the call-site side (is_error = o.get("error").is_some() at
    /// pipeline.rs) keeps the synthetic json honest.
    #[test]
    fn test_retry_after_interrupt() {
        let mut t = RedundancyTracker::new();
        // The Interrupted synthetic is is_error=true at the call site, so the
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
        t.check_batch(std::iter::once(("bash", &input)));
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
        t.check_batch(std::iter::once(("bash", &input)));
        assert_eq!(t.retry_after_error(), 0);
    }

    /// A retry of a prior success (no write) is redundant, not a blind
    /// retry — retry_after_error stays 0 and the redundant record is flagged.
    #[test]
    fn test_blind_retry_zero_redundant() {
        let mut t = RedundancyTracker::new();
        let input = json!({"file_path": "src/lib.rs"});
        t.record("read", &input, false);
        t.check_batch(std::iter::once(("read", &input)));
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
        t.check_batch(std::iter::once(("bash", &input)));
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
            t.check_batch(std::iter::once(("bash", &input)));
            t.record("bash", &input, true);
        }
        assert!(
            t.retry_tools().len() <= REDUNDANCY_RECORDS_CAP,
            "retry_tools bounded: {}",
            t.retry_tools().len()
        );
    }
}
