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
use std::str;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use super::Tokenizer;
use crate::agent::context::cas_views::CasViewState;
use houyicoder_context::{
    BlockHash, ContextBackend, ContextError, SessionEvent, SessionLogEntry, ViewForm,
};
use houyicoder_protocol::block_ref::BLOCK_BYTES_KEY;
use houyicoder_protocol::block_ref::BLOCK_REF_KEY;
use houyicoder_protocol::block_ref::HINT_KEY;
use houyicoder_protocol::block_ref::PREVIEW_KEY;
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
    /// superseded result; the hint points the model at the stored result it
    /// can search for.
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
    /// The stored byte length the marker names for this block, when it
    /// carries one. A budget policy uses this as a pre-fetch token upper
    /// bound (a UTF-8 byte count is never below the token count) so it can
    /// decide to demote without reading the block first.
    pub stored_bytes: Option<u64>,
    /// The tokens this assembly already counts for the result as the log
    /// holds it, when a plan outside the retention path paid for that form.
    /// A retrieval replaces the logged form, so the budget check drops this
    /// term before adding the stored length: the two forms stand in for one
    /// another and must not both spend the assembly's quota.
    pub logged_result_tokens: Option<u64>,
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

    /// Report the stored bytes of a block the policy materialized, so a
    /// token-budget policy can tokenize the actual content and account it in
    /// the budget's unit. The default ignores it; a byte-count policy keeps
    /// using record_materialized_bytes.
    fn record_materialized_content(&self, _block_ref: &str, _content: &[u8]) {}

    /// Set the per-turn materialization budget in tokens. The assembler calls
    /// this each turn after measuring the window so a budget policy gates on a
    /// window-proportional ceiling rather than a fixed constant. The default
    /// ignores it (non-budgeted policies do not gate).
    fn set_turn_budget(&self, _tokens: u32) {}

    /// The tokens one request may spend on the history of user-typed shell
    /// commands. The default is the fixed ceiling: every assembly bounds the
    /// command history, and a policy that knows the model window tightens it.
    fn shell_history_budget(&self) -> u32 {
        TURN_RESULT_BUDGET_CAP_TOKENS
    }

    /// Count request text in the budget's unit. The default counts bytes,
    /// which no token count exceeds, so an assembly without a tokenizer still
    /// folds at an upper bound rather than overshooting.
    fn count_tokens(&self, text: &str) -> usize {
        text.len()
    }

    /// Charge tokens the assembly commits outside the retention path against
    /// this assembly's quota, so text the request already carries cannot be
    /// spent twice. The charge replaces the previous one for the same
    /// assembly, so measuring the same assembly again does not consume the
    /// quota a second time. The default ignores it.
    fn charge_assembly_tokens(&self, _assembly_id: u64, _tokens: u32) {}

    /// The record the assembly reports the forms it carried to, or None for
    /// a policy that keeps no such record. The default reports nothing,
    /// which a reader sees as an unknown model view rather than an assembly
    /// that carried no blocks.
    fn cas_views(&self) -> Option<&CasViewState> {
        None
    }
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

/// The cap on the per-turn tool-result budget, in tokens. The design sets
/// B = min(8192, floor(0.10 * W)) where W is the window left after system,
/// tools, user input, and the reply reserve. 8192 is the hard ceiling; the
/// assembler computes the window-proportional half each turn and calls
/// set_turn_budget, so the budget tracks the model window rather than a fixed
/// constant. The load experiment may lower the fraction; it does not change
/// the ceiling or the token unit.
pub const TURN_RESULT_BUDGET_CAP_TOKENS: u32 = 8192;

/// A retention policy that bounds how many tokens a single turn materializes
/// from the CAS. It wraps an inner policy (age or cache-liveness) and demotes a
/// Materialize decision to Summarize once the tokens the turn has already
/// retrieved reach the per-turn budget. The counter resets at an assembly
/// boundary, so each turn starts with its full quota.
///
/// The decision is made before the block is read. The pre-check uses the
/// stored byte length the marker names as a token upper bound (a UTF-8 byte
/// count is never below the token count), so a block that would push the turn
/// past its budget is demoted without a retrieval. The final accounting
/// tokenizes the materialized content, so the accumulator is exact once a
/// block is read, not a byte proxy.
pub struct BudgetedRetentionPolicy {
    inner: Arc<dyn RetentionPolicy>,
    tokenizer: Tokenizer,
    budget_tokens: AtomicU32,
    accumulated: AtomicU64,
    charged: AtomicU64,
    assembly: AtomicU64,
    /// Where the assembler reports the forms it carried, once the host
    /// installs the record. None in tests that only exercise the budget.
    cas_views: Option<Arc<CasViewState>>,
}

impl BudgetedRetentionPolicy {
    pub fn new(inner: Arc<dyn RetentionPolicy>, tokenizer: Tokenizer) -> Self {
        Self {
            inner,
            tokenizer,
            budget_tokens: AtomicU32::new(TURN_RESULT_BUDGET_CAP_TOKENS),
            accumulated: AtomicU64::new(0),
            charged: AtomicU64::new(0),
            assembly: AtomicU64::new(u64::MAX),
            cas_views: None,
        }
    }

    /// Install the record the assembler reports its forms to. Called before
    /// the policy is shared, so every assembly reports to one record.
    pub fn set_cas_views(&mut self, views: Arc<CasViewState>) {
        self.cas_views = Some(views);
    }

    /// Start this assembly's quota over when it is not the one the counters
    /// were left on. Both halves reset together: the tokens the assembler
    /// committed and the ones the retention path retrieved belong to one
    /// assembly, and a half-reset quota would either over- or under-count it.
    fn start_assembly(&self, assembly_id: u64) {
        if self.assembly.swap(assembly_id, Ordering::Relaxed) != assembly_id {
            self.accumulated.store(0, Ordering::Relaxed);
            self.charged.store(0, Ordering::Relaxed);
        }
    }

    /// Drop the tokens the assembly holds for a logged form the retrieval
    /// replaces. The retrieved content is accounted by
    /// record_materialized_content as it is read, so a hold left in place
    /// would spend one result's quota twice. A hold a later assembly already
    /// lowered saturates at zero rather than wrapping.
    fn release_charge(&self, tokens: u64) {
        if tokens == 0 {
            return;
        }
        let _ = self
            .charged
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |held| {
                Some(held.saturating_sub(tokens))
            });
    }
}

impl RetentionPolicy for BudgetedRetentionPolicy {
    fn decide(&self, ctx: &RetentionContext<'_>) -> RetentionDecision {
        let decision = self.inner.decide(ctx);
        if decision == RetentionDecision::Materialize {
            // The reset is checked here rather than on every call so a turn
            // whose first blocks summarize does not touch the counters until
            // one wants to materialize.
            self.start_assembly(ctx.assembly_id);
            let budget = self.budget_tokens.load(Ordering::Relaxed) as u64;
            // Pre-check: the stored byte length is a token upper bound, so a
            // block that would push the turn past its budget is demoted before
            // the retrieval. A result the assembly already counts for its
            // logged form has that term dropped first: the retrieval replaces
            // the logged text, so counting both would spend one result's
            // quota twice. A marker without a stored length is decided on the
            // accumulated total alone (the inner policy already bounds single
            // blocks upstream via the isolate and bash caps).
            let replaced = ctx.logged_result_tokens.unwrap_or(0);
            let estimated = self.accumulated.load(Ordering::Relaxed)
                + self
                    .charged
                    .load(Ordering::Relaxed)
                    .saturating_sub(replaced)
                + ctx.stored_bytes.unwrap_or(0);
            if estimated >= budget {
                return RetentionDecision::Summarize;
            }
            self.release_charge(replaced);
        }
        decision
    }

    fn record_materialized_bytes(&self, block_ref: &str, bytes: usize) {
        self.inner.record_materialized_bytes(block_ref, bytes);
    }

    fn record_materialized_content(&self, block_ref: &str, content: &[u8]) {
        self.inner.record_materialized_content(block_ref, content);
        // Final accounting: tokenize the actual materialized content so the
        // accumulator shares the budget's token unit, not a byte proxy. The
        // stored bytes are a UTF-8 JSON serialization; a decode failure falls
        // back to the byte count as a conservative upper bound.
        let tokens = str::from_utf8(content)
            .map(|s| u64::from(self.tokenizer.count(s)))
            .unwrap_or(content.len() as u64);
        self.accumulated.fetch_add(tokens, Ordering::Relaxed);
    }

    fn set_turn_budget(&self, tokens: u32) {
        self.budget_tokens.store(tokens, Ordering::Relaxed);
    }

    fn shell_history_budget(&self) -> u32 {
        self.budget_tokens.load(Ordering::Relaxed)
    }

    fn count_tokens(&self, text: &str) -> usize {
        self.tokenizer.count(text) as usize
    }

    fn charge_assembly_tokens(&self, assembly_id: u64, tokens: u32) {
        self.start_assembly(assembly_id);
        self.charged.store(u64::from(tokens), Ordering::Relaxed);
    }

    fn cas_views(&self) -> Option<&CasViewState> {
        self.cas_views.as_deref()
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
/// in the CAS, unreachable until a backend is available — not lost).
///
/// A mutating assembly reports each form it carried to the policy's view
/// record, so the session log states what the request carried. A
/// measurement does not report, because nothing was sent.
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
    // key a per-block decision on it, and the stored byte length the marker
    // names so a budget policy can pre-check the size without fetching.
    let ctx = RetentionContext {
        block_ref: Some(hash_str),
        stored_bytes: marker.get(BLOCK_BYTES_KEY).and_then(Value::as_u64),
        ..*ctx
    };
    // A measurement reads the same blocks without recording, so only a
    // request the provider will receive pays for the size count.
    let recording = ctx.mutating && policy.cas_views().is_some();
    // The form and its size come back together: the recorded view must
    // name what the model actually received, which can differ from the
    // decision (a marker without a preview serves the pointer).
    let (value, form, size) = match policy.decide(&ctx) {
        RetentionDecision::Materialize => {
            let hash = BlockHash(hash_str.to_string());
            match pollster::block_on(backend.block_get(&hash)) {
                Ok(bytes) => match serde_json::from_slice(&bytes) {
                    Ok(value) => {
                        policy.record_materialized_bytes(hash_str, bytes.len());
                        policy.record_materialized_content(hash_str, &bytes);
                        // The stored bytes are the JSON the model receives
                        // here, so the figure counts them rather than
                        // serializing the value again.
                        let size = if recording {
                            str::from_utf8(&bytes)
                                .map(|text| policy.count_tokens(text) as u64)
                                .unwrap_or(bytes.len() as u64)
                        } else {
                            0
                        };
                        (value, ViewForm::Materialize, size)
                    }
                    // Stored bytes did not deserialize: the block is corrupt or
                    // schema-drifted. Surface a typed unavailable marker so the
                    // model knows the content is unreachable, not absent.
                    Err(_) => unavailable_view(
                        policy,
                        hash_str,
                        "block content corrupt or unreadable",
                        recording,
                    ),
                },
                // A block whose bytes do not match its address is corrupt, not
                // missing: re-invoking the tool would store the bytes again,
                // not recover them, so the model is told the content is
                // unreachable.
                Err(ContextError::Corrupt(_)) => unavailable_view(
                    policy,
                    hash_str,
                    "block content corrupt or unreadable",
                    recording,
                ),
                Err(_) => unavailable_view(
                    policy,
                    hash_str,
                    "block_get failed; the stored content could not be read",
                    recording,
                ),
            }
        }
        RetentionDecision::Summarize => {
            // Serve the inline preview the marker carries; skip the block_get.
            // A marker without a preview (an old-style marker) falls back to
            // the pointer form rather than paying a retrieval. Both forms
            // point the model at the stored result instead of telling it to
            // run the command again: the content is already in the log, and a
            // second run may not reproduce it.
            if marker.get(PREVIEW_KEY).is_some() {
                let value = serde_json::json!({
                    "summarized": true,
                    PREVIEW_KEY: marker.get(PREVIEW_KEY).cloned().unwrap_or_default(),
                    BLOCK_REF_KEY: hash_str,
                    HINT_KEY: "large output summarized; search the conversation to read it",
                });
                let size = view_size(policy, &value, recording);
                (value, ViewForm::Summarize, size)
            } else {
                // The pointer is what reached the model, so that is the form
                // the record names, not the decision that chose it.
                let value = marker.clone();
                let size = view_size(policy, &value, recording);
                (value, ViewForm::Evict, size)
            }
        }
        RetentionDecision::Evict => {
            // The pointer alone, typed evicted so the model can distinguish
            // "removed by policy" from "backend cannot retrieve." No
            // retrieval; the hint names the stored result the model can
            // search for.
            let value = serde_json::json!({
                BLOCK_REF_KEY: hash_str,
                "evicted": true,
                HINT_KEY: "large output left out of this request; search the conversation to read it",
            });
            let size = view_size(policy, &value, recording);
            (value, ViewForm::Evict, size)
        }
    };
    if recording
        && let Some(views) = policy.cas_views()
        && let Some(block_ref) = ctx.block_ref
    {
        views.record(ctx.assembly_id, block_ref, form, size);
    }
    value
}

/// The assembled size of one carried value, in the policy's counting unit.
/// A measurement asks for no size, so it pays no tokenizer call.
fn view_size(policy: &dyn RetentionPolicy, value: &serde_json::Value, recording: bool) -> u64 {
    if !recording {
        return 0;
    }
    // A JSON value always serializes; the fallback covers only a serializer
    // this call cannot observe failing.
    let text = serde_json::to_string(value).unwrap_or_default();
    policy.count_tokens(&text) as u64
}

/// The form, value, and size for a block whose stored content could not be
/// read.
fn unavailable_view(
    policy: &dyn RetentionPolicy,
    hash_str: &str,
    reason: &str,
    recording: bool,
) -> (serde_json::Value, ViewForm, u64) {
    let value = unavailable_marker(hash_str, reason);
    let size = view_size(policy, &value, recording);
    (value, ViewForm::Unavailable, size)
}

/// A typed marker for a block the assembly could not materialize (backend
/// failure, corrupt bytes). Carries the block_ref so a later read can still
/// resolve it, and a reason so the model knows the content is unreachable,
/// not absent. The turn does not fail; the hint says plainly that the stored
/// content could not be read, in place of an action that would not work.
fn unavailable_marker(hash_str: &str, reason: &str) -> serde_json::Value {
    serde_json::json!({
        BLOCK_REF_KEY: hash_str,
        "unavailable": true,
        "reason": reason,
        HINT_KEY: "large output unavailable; its stored content could not be read",
    })
}

#[cfg(test)]
mod tests;
