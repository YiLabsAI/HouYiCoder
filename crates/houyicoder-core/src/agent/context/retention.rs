//! Retention: CAS-backed storage of large ToolResult outputs.
//!
//! Two operations on the retention seam:
//! - materialize_block: decide how a block_ref ToolResult output is served.
//!   A 3-tier policy chooses materialize (retrieve the full content for a
//!   recent, active result), summarize (serve a small inline preview, no
//!   retrieval), or evict (serve the pointer alone, no retrieval). The
//!   default AgeRetentionPolicy keys the decision to how many turns ago the
//!   result was produced and whether a later result superseded it.
//! - isolate_large_output (in append.rs): serialize a tool output, store it
//!   via block_put, and return a block_ref marker (with an inline preview)
//!   so the raw large content is not in the assembled context. A structured result
//!   externalizes only its largest string field, keeping the envelope's
//!   other keys inline. Fail-closed: no backend or block_put failure keeps
//!   the original output (no content loss, no dangling marker).
//!
//! This is the lossless layer (layer 3) of the compress design: the served
//! view carries a small pointer or preview; the full content is addressable
//! by hash on demand. Cross-session dedup falls out of content addressing
//! (same hash not rewritten).
//!
//! The 3-tier policy replaces the prior unconditional materialize, which
//! re-read every block_ref into the view — zero assembled-context savings, the CAS
//! stored bytes for nothing. Summarize and evict skip the block_get so the
//! verbatim tail stays small; only a recent, active result pays the
//! retrieval. A future policy can add cache-liveness (clear aggressively
//! when the cache is already dead) and TTL, using the provider
//! microcompact vocabulary (trigger/keep/clear_at_least).

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use houyicoder_context::{BlockHash, ContextBackend, ContextError, SessionEvent, SessionLogEntry};
use houyicoder_protocol::block_ref::BLOCK_REF_KEY;
use serde_json::Value;

/// A resource key for supersession: a later tool call for the same resource
/// supersedes the earlier result. The key is the file path for file tools +
/// the command for bash. Returns None when the call has no comparable key,
/// so two unrelated calls never mark each other superseded.
fn resource_key(tool: &str, input: &Value) -> Option<String> {
    match tool {
        "read" | "write" | "edit" | "multiedit" => input
            .get("path")
            .and_then(|v| v.as_str())
            .map(|p| p.to_string()),
        "bash" => input
            .get("command")
            .and_then(|v| v.as_str())
            .map(|c| c.to_string()),
        _ => None,
    }
}

/// A mutating tool changes the resource, so its result supersedes any prior
/// result for that resource (a prior read is now stale). A read is not a
/// mutation — a read after an edit does not supersede the edit (the edit's
/// effect is permanent).
fn is_mutation(tool: &str) -> bool {
    matches!(tool, "write" | "edit" | "multiedit")
}

/// Extract a range parameter as a normalized string so numeric and string
/// representations compare equal.
fn range_field(input: &Value, key: &str) -> Option<String> {
    match input.get(key)? {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        v => Some(v.to_string()),
    }
}

/// The line-window and byte-cap parameters of a read call. None in all three
/// positions means an unbounded full-file read.
fn read_range(input: &Value) -> (Option<String>, Option<String>, Option<String>) {
    let limit = range_field(input, "limit").or_else(|| range_field(input, "context"));
    (
        range_field(input, "offset"),
        limit,
        range_field(input, "max_bytes"),
    )
}

/// Whether a later read of the same path covers the lines of an earlier read:
/// a full-file read covers any slice, and an identical slice covers itself,
/// while a disjoint slice leaves the earlier lines as the only copy in view.
fn read_covers(own_input: &Value, later_input: &Value) -> bool {
    let later = read_range(later_input);
    later == (None, None, None) || later == read_range(own_input)
}

/// Whether a later tool call in the window supersedes the result at index i.
/// A result is superseded when a later answered call touches the same resource
/// and either mutates it or re-reads a covering range.
pub(crate) fn superseded_by_later(events: &[SessionLogEntry], i: usize) -> bool {
    let SessionEvent::ToolResult { call_id, .. } = &events[i].event else {
        return false;
    };
    let mut own: Option<(&str, &Value, String)> = None;
    for ev in events[..i].iter().rev() {
        if let SessionEvent::ToolCall {
            call_id: cid,
            tool,
            input,
        } = &ev.event
            && cid == call_id
        {
            own = resource_key(tool, input).map(|k| (tool.as_str(), input, k));
            break;
        }
    }
    let Some((own_tool, own_input, key)) = own else {
        return false;
    };
    // Only a call whose result is also in the window has replaced the older
    // result. A pending call has not answered yet, so the older content is
    // still the best copy the model can read; evicting it early would lose
    // content on a call that fails or is cancelled.
    let answered: HashSet<&str> = events[i + 1..]
        .iter()
        .filter_map(|ev| match &ev.event {
            SessionEvent::ToolResult { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    events[i + 1..].iter().any(|ev| match &ev.event {
        SessionEvent::ToolCall {
            call_id: later_call,
            tool,
            input,
        } => {
            let Some(later_key) = resource_key(tool, input) else {
                return false;
            };
            let later_tool = tool.as_str();
            let same_tool_covers =
                later_tool == own_tool && (own_tool != "read" || read_covers(own_input, input));
            later_key == key
                && (is_mutation(later_tool) || same_tool_covers)
                && answered.contains(later_call.as_str())
        }
        _ => false,
    })
}

/// How a block_ref ToolResult reaches the assembled model input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetentionDecision {
    /// Retrieve the full content from the CAS — for a recent, active result
    /// the model needs in full.
    Materialize,
    /// Serve the inline preview stored in the marker, no retrieval — for a
    /// middle-aged result the model rarely revisits in full.
    Summarize,
    /// Serve the block_ref pointer alone, no retrieval — for an old or
    /// superseded result; the model is told to re-invoke the tool if it needs
    /// the content.
    Evict,
}

impl RetentionDecision {
    /// The label recorded in the session log. A log carries the label rather
    /// than the enum so the context crate stays independent of this policy.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Materialize => "materialize",
            Self::Summarize => "summarize",
            Self::Evict => "evict",
        }
    }

    /// Parse a label a log recorded. An unknown label is None rather than a
    /// guessed tier, so a record from a newer build is skipped.
    pub(crate) fn from_label(label: &str) -> Option<Self> {
        match label {
            "materialize" => Some(Self::Materialize),
            "summarize" => Some(Self::Summarize),
            "evict" => Some(Self::Evict),
            _ => None,
        }
    }
}

/// The assembly a retention decision belongs to. A measurement reuses the
/// turn id with mutating false, so a policy can tell a request from a view.
#[derive(Debug, Clone, Copy)]
pub struct AssemblyScope {
    pub id: u64,
    pub mutating: bool,
}

/// The context a retention policy decides on. The unit of age is the API
/// round (one assistant turn): age 0 is the most recent round, age 1 one
/// round back, and so on. A superseded result was replaced by a later result
/// for the same resource (a newer read of the same file, a retry that
/// succeeded) and is prioritized for eviction regardless of age. The block_ref
/// is the CAS hash of the ToolResult output (None when the output carries no
/// block_ref marker) so a cache-liveness policy can hold a per-block decision
/// stable across turns. The assembly id names the one assembly a decision
/// belongs to, so a policy can scope a batch recompute to a turn boundary.
#[derive(Debug, Clone, Copy)]
pub struct RetentionContext<'a> {
    /// How many assistant turns ago this result was produced.
    pub age_in_turns: u32,
    /// Whether a later result replaced this one.
    pub is_superseded: bool,
    /// The CAS hash when the output is a block_ref marker, else None.
    pub block_ref: Option<&'a str>,
    /// The assembly this decision is computed for.
    pub assembly_id: u64,
    /// Whether this assembly will be sent to the provider.
    pub mutating: bool,
}

/// The retention policy seam. The default AgeRetentionPolicy keys the
/// decision to age and supersession; the cache-liveness policy holds a
/// per-block decision stable across turns while the cache is live.
pub trait RetentionPolicy: Send + Sync {
    fn decide(&self, ctx: &RetentionContext<'_>) -> RetentionDecision;

    /// Report the byte size of a block the policy materialized, so a policy
    /// that measures the context it holds can size the held set. The default
    /// ignores the observation.
    fn record_materialized_bytes(&self, _block_ref: &str, _bytes: usize) {}
}

/// Default policy: superseded results evict regardless of age; recent
/// results materialize; middle-aged results summarize; old results evict.
/// The thresholds are conservative so a result the model is actively working
/// with stays full while older context folds toward previews and pointers.
pub struct AgeRetentionPolicy {
    /// Results younger than this (in turns) materialize in full.
    pub materialize_turns: u32,
    /// Results younger than this summarize; older ones evict.
    pub summarize_turns: u32,
}

impl Default for AgeRetentionPolicy {
    fn default() -> Self {
        Self {
            materialize_turns: 2,
            summarize_turns: 6,
        }
    }
}

impl RetentionPolicy for AgeRetentionPolicy {
    fn decide(&self, ctx: &RetentionContext<'_>) -> RetentionDecision {
        if ctx.is_superseded {
            return RetentionDecision::Evict;
        }
        if ctx.age_in_turns < self.materialize_turns {
            RetentionDecision::Materialize
        } else if ctx.age_in_turns < self.summarize_turns {
            RetentionDecision::Summarize
        } else {
            RetentionDecision::Evict
        }
    }
}

/// The most bytes of CAS blocks one turn may materialize back into the
/// assembled context. A byte proxy for the design's per-turn tool-result
/// budget B = min(8192, 0.10 * W) tokens: 32 KiB covers about 8 KiB of tokens
/// at four bytes each, the floor the design sets before the load experiment
/// picks a window-proportional value. The budget bounds the cumulative
/// retrieval, not any single block, so a recent active result still enters in
/// full while older ones fold to previews once the turn has spent its quota.
pub const TURN_RESULT_BUDGET_BYTES: u64 = 32 * 1024;

/// A retention policy that bounds how many bytes a single turn materializes
/// from the CAS. It wraps an inner policy (age or cache-liveness) and demotes a
/// Materialize decision to Summarize once the bytes the turn has already
/// retrieved reach the budget. The counter resets at an assembly boundary, so
/// each turn starts with its full quota. The decision is made before the block
/// is read: the accumulator holds what prior blocks in the same turn cost, not
/// a pre-fetch of the block being decided, so the gate never reads the full
/// content only to reject it.
pub struct BudgetedRetentionPolicy {
    inner: Arc<dyn RetentionPolicy>,
    max_bytes: u64,
    accumulated: AtomicU64,
    assembly: AtomicU64,
}

impl BudgetedRetentionPolicy {
    pub fn new(inner: Arc<dyn RetentionPolicy>, max_bytes: u64) -> Self {
        Self {
            inner,
            max_bytes,
            accumulated: AtomicU64::new(0),
            assembly: AtomicU64::new(u64::MAX),
        }
    }
}

impl RetentionPolicy for BudgetedRetentionPolicy {
    fn decide(&self, ctx: &RetentionContext<'_>) -> RetentionDecision {
        let decision = self.inner.decide(ctx);
        if decision == RetentionDecision::Materialize {
            // A new assembly starts the quota over. The reset is checked here
            // rather than on every call so a turn whose first blocks summarize
            // does not touch the counter until one wants to materialize.
            if self.assembly.swap(ctx.assembly_id, Ordering::Relaxed) != ctx.assembly_id {
                self.accumulated.store(0, Ordering::Relaxed);
            }
            if self.accumulated.load(Ordering::Relaxed) >= self.max_bytes {
                return RetentionDecision::Summarize;
            }
        }
        decision
    }

    fn record_materialized_bytes(&self, block_ref: &str, bytes: usize) {
        self.inner.record_materialized_bytes(block_ref, bytes);
        self.accumulated.fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

/// Decide how a block_ref ToolResult output is served. When the output
/// carries a block_ref marker and a backend is available, the policy picks
/// materialize (retrieve full content), summarize (serve the inline
/// preview), or evict (serve the pointer alone). Outputs without a block_ref
/// key pass through unchanged. A marker with no preview field falls back to
/// the pointer form under Summarize (the preview is the savings; without it
/// there is nothing to summarize to, and paying a retrieval would defeat the
/// tier).
pub(crate) fn materialize_block(
    output: &serde_json::Value,
    backend: Option<&dyn ContextBackend>,
    policy: &dyn RetentionPolicy,
    ctx: &RetentionContext<'_>,
) -> serde_json::Value {
    // Top-level marker: the whole output was externalized (blob tools like
    // bash/grep, or old-session logs written before field-level isolation).
    // The tier replaces the whole output — these have no envelope to keep.
    if let Some(hash_str) = output.get(BLOCK_REF_KEY).and_then(|v| v.as_str()) {
        return apply_tier(hash_str, output, backend, policy, ctx);
    }
    // Field-level marker: a single top-level field's value is the marker
    // object (isolate_large_output externalized the largest string field so
    // the envelope's other keys — agentId, color, status, usage — stay
    // inline). Apply the tier to that field, preserve every other key, so
    // the model can still reference the child session and the TUI still
    // renders the Subagent fold-group across all three retention tiers.
    if let Some(obj) = output.as_object()
        && let Some((field_key, marker)) = obj.iter().find_map(|(k, v)| {
            let hash = v.get(BLOCK_REF_KEY).and_then(|b| b.as_str())?;
            Some((k.clone(), (hash, v.clone())))
        })
    {
        let new_field = apply_tier(marker.0, &marker.1, backend, policy, ctx);
        let mut out = output.clone();
        out[field_key] = new_field;
        return out;
    }
    output.clone()
}

/// Apply a retention tier to a block_ref marker, returning the value that
/// replaces the marker in its scope (the whole output for a top-level
/// marker, the field for a field-level marker). Materialize restores the
/// original content via block_get; Summarize serves the inline preview;
/// Evict keeps only the pointer. No backend: the marker stays (content is
/// in the CAS, unreachable until a backend is wired — not lost).
fn apply_tier(
    hash_str: &str,
    marker: &serde_json::Value,
    backend: Option<&dyn ContextBackend>,
    policy: &dyn RetentionPolicy,
    ctx: &RetentionContext<'_>,
) -> serde_json::Value {
    let Some(backend) = backend else {
        return marker.clone();
    };
    // Stamp the block_ref onto the context so a cache-liveness policy can
    // key a per-block decision on it.
    let ctx = RetentionContext {
        block_ref: Some(hash_str),
        ..*ctx
    };
    match policy.decide(&ctx) {
        RetentionDecision::Materialize => {
            let hash = BlockHash(hash_str.to_string());
            match pollster::block_on(backend.block_get(&hash)) {
                Ok(bytes) => match serde_json::from_slice(&bytes) {
                    Ok(value) => {
                        policy.record_materialized_bytes(hash_str, bytes.len());
                        value
                    }
                    // Stored bytes did not deserialize: the block is corrupt or
                    // schema-drifted. Surface a typed unavailable marker so the
                    // model knows the content is unreachable, not absent.
                    Err(_) => unavailable_marker(hash_str, "block content corrupt or unreadable"),
                },
                // A block whose bytes do not match its address is corrupt, not
                // missing: re-invoking the tool would store the bytes again,
                // not recover them, so the model is told the content is
                // unreachable.
                Err(ContextError::Corrupt(_)) => {
                    unavailable_marker(hash_str, "block content corrupt or unreadable")
                }
                Err(_) => unavailable_marker(hash_str, "block_get failed; re-invoke the tool"),
            }
        }
        RetentionDecision::Summarize => {
            // Serve the inline preview the marker carries; skip the block_get.
            // A marker without a preview (an old-style marker) falls back to
            // the pointer form rather than paying a retrieval — the model is
            // told to re-invoke.
            if marker.get("preview").is_some() {
                serde_json::json!({
                    "summarized": true,
                    "preview": marker.get("preview").cloned().unwrap_or_default(),
                    BLOCK_REF_KEY: hash_str,
                    "hint": "large output summarized; re-invoke the tool for full content",
                })
            } else {
                marker.clone()
            }
        }
        RetentionDecision::Evict => {
            // The pointer alone, typed evicted so the model can distinguish
            // "removed by policy" from "backend cannot retrieve." No
            // retrieval; the model is told to re-invoke if it needs the
            // content.
            serde_json::json!({
                BLOCK_REF_KEY: hash_str,
                "evicted": true,
                "hint": "large output evicted by retention policy; re-invoke the tool to retrieve it",
            })
        }
    }
}

/// A typed marker for a block the assembly could not materialize (backend
/// failure, corrupt bytes). Carries the block_ref so a later re-invoke can
/// still resolve it, and a reason so the model knows the content is
/// unreachable, not absent. The turn does not fail — the model is told to
/// re-invoke the tool.
fn unavailable_marker(hash_str: &str, reason: &str) -> serde_json::Value {
    serde_json::json!({
        BLOCK_REF_KEY: hash_str,
        "unavailable": true,
        "reason": reason,
        "hint": "large output unavailable; re-invoke the tool to retrieve it",
    })
}

#[cfg(test)]
mod tests;
