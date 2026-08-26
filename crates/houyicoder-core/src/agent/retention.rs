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
//!   so the raw large content is not in the served view. A structured result
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
//! re-read every block_ref into the view — zero served-view savings, the CAS
//! stored bytes for nothing. Summarize and evict skip the block_get so the
//! verbatim tail stays small; only a recent, active result pays the
//! retrieval. A future policy can add cache-liveness (clear aggressively
//! when the cache is already dead) and TTL, using the provider
//! microcompact vocabulary (trigger/keep/clear_at_least).

use houyicoder_context::{BlockHash, ContextBackend, TurnEvent, TurnEventKind};

/// A resource key for supersession: a later tool call for the same resource
/// supersedes the earlier result. The key is the file path for file tools +
/// the command for bash. Returns None when the call has no comparable key,
/// so two unrelated calls never mark each other superseded.
fn resource_key(tool: &str, input: &serde_json::Value) -> Option<String> {
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

/// Whether a later tool call in the window supersedes the result at index i.
/// A result is superseded when a later call touches the same resource AND
/// either is a mutation (the resource changed, so any prior result is stale)
/// or is the same tool (a re-read, a re-run — the newer result wins). The
/// result's own ToolCall is found by scanning backward for the call_id.
/// A superseded result is prioritized for eviction regardless of age.
pub(super) fn superseded_by_later(events: &[TurnEvent], i: usize) -> bool {
    let TurnEventKind::ToolResult { call_id, .. } = &events[i].kind else {
        return false;
    };
    // Find the matching ToolCall (scan backward from the result) + its key.
    let mut own: Option<(&str, String)> = None;
    for ev in events[..i].iter().rev() {
        if let TurnEventKind::ToolCall {
            call_id: cid,
            tool,
            input,
        } = &ev.kind
            && cid == call_id
        {
            own = resource_key(tool, input).map(|k| (tool.as_str(), k));
            break;
        }
    }
    let Some((own_tool, key)) = own else {
        return false; // not resource-keyed, never superseded by this rule
    };
    events[i + 1..].iter().any(|ev| match &ev.kind {
        TurnEventKind::ToolCall { tool, input, .. } => {
            let Some(later_key) = resource_key(tool, input) else {
                return false;
            };
            let later_tool = tool.as_str();
            later_key == key && (is_mutation(later_tool) || later_tool == own_tool)
        }
        _ => false,
    })
}

/// How a block_ref ToolResult is served in the projected view.
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

/// The context a retention policy decides on. The unit of age is the API
/// round (one assistant turn): age 0 is the most recent round, age 1 one
/// round back, and so on. A superseded result was replaced by a later result
/// for the same resource (a newer read of the same file, a retry that
/// succeeded) and is prioritized for eviction regardless of age. The block_ref
/// is the CAS hash of the ToolResult output (None when the output carries no
/// block_ref marker) so a cache-liveness policy can hold a per-block decision
/// stable across turns. The wall-clock now_ms lets it test the cache TTL.
#[derive(Debug, Clone, Copy)]
pub struct RetentionContext<'a> {
    /// How many assistant turns ago this result was produced.
    pub age_in_turns: u32,
    /// Whether a later result replaced this one.
    pub is_superseded: bool,
    /// The CAS hash when the output is a block_ref marker, else None.
    pub block_ref: Option<&'a str>,
    /// Wall-clock milliseconds at serve time (for the cache-liveness TTL test).
    pub now_ms: u64,
    /// True when the provider's prompt cache is cold (the gap since the last
    /// assistant message exceeds the cache TTL). When cold, recent results
    /// that would normally materialize in full are downgraded to Summarize
    /// — the cached prefix is being rewritten anyway, so shrinking it first
    /// saves tokens without losing a cache hit. A time-based microcompact
    /// trigger.
    pub cache_cold: bool,
}

/// The retention policy seam. The default AgeRetentionPolicy keys the
/// decision to age and supersession; the cache-liveness policy holds a
/// per-block decision stable across turns while the cache is live.
pub trait RetentionPolicy: Send + Sync {
    fn decide(&self, ctx: &RetentionContext<'_>) -> RetentionDecision;
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
        // When the cache is cold (TTL expired), downgrade recent results
        // from Materialize to Summarize — the prefix is being rewritten
        // anyway, so shrinking it first saves tokens without losing a hit.
        if ctx.cache_cold {
            if ctx.age_in_turns < self.summarize_turns {
                return RetentionDecision::Summarize;
            }
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

/// Decide how a block_ref ToolResult output is served. When the output
/// carries a block_ref marker and a backend is available, the policy picks
/// materialize (retrieve full content), summarize (serve the inline
/// preview), or evict (serve the pointer alone). Outputs without a block_ref
/// key pass through unchanged. A marker with no preview field falls back to
/// the pointer form under Summarize (the preview is the savings; without it
/// there is nothing to summarize to, and paying a retrieval would defeat the
/// tier).
pub(super) fn materialize_block(
    output: &serde_json::Value,
    backend: Option<&dyn ContextBackend>,
    policy: &dyn RetentionPolicy,
    ctx: &RetentionContext<'_>,
) -> serde_json::Value {
    // Top-level marker: the whole output was externalized (blob tools like
    // bash/grep, or old-session logs written before field-level isolation).
    // The tier replaces the whole output — these have no envelope to keep.
    if let Some(hash_str) = output.get("block_ref").and_then(|v| v.as_str()) {
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
            let hash = v.get("block_ref").and_then(|b| b.as_str())?;
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
                Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|_| {
                    // Stored bytes did not deserialize: the block is corrupt or
                    // schema-drifted. Surface a typed unavailable marker so the
                    // model knows the content is unreachable, not absent.
                    unavailable_marker(hash_str, "block content corrupt or unreadable")
                }),
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
                    "block_ref": hash_str,
                    "hint": "large output summarized; re-invoke the tool for full content",
                })
            } else {
                marker.clone()
            }
        }
        RetentionDecision::Evict => {
            // The pointer alone, typed evicted so the model can distinguish
            // "retired by policy" from "backend cannot retrieve." No
            // retrieval; the model is told to re-invoke if it needs the
            // content.
            serde_json::json!({
                "block_ref": hash_str,
                "evicted": true,
                "hint": "large output evicted by retention policy; re-invoke the tool to retrieve it",
            })
        }
    }
}

/// A typed marker for a block the projection could not materialize (backend
/// failure, corrupt bytes). Carries the block_ref so a later re-invoke can
/// still resolve it, and a reason so the model knows the content is
/// unreachable, not absent. The turn does not fail — the model is told to
/// re-invoke the tool.
fn unavailable_marker(hash_str: &str, reason: &str) -> serde_json::Value {
    serde_json::json!({
        "block_ref": hash_str,
        "unavailable": true,
        "reason": reason,
        "hint": "large output unavailable; re-invoke the tool to retrieve it",
    })
}

#[cfg(test)]
#[path = "retention_tests.rs"]
mod tests;
