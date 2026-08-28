//! agent::manifest — the Compress stage's disposition planner.
//!
//! Given the append-only event log and a CompressPolicy, build_manifest
//! produces a CheckpointManifest: a per-turn-group Disposition plan
//! (Verbatim, Summarized) plus a summary of the folded span. The raw log is
//! never mutated; applying the plan to a replay yields the served window.
//!
//! Disposition heuristic:
//! - The last tail_turns assistant turns (API rounds) stay Verbatim (preserved
//!   in the view). The unit is the assistant response so a single-user agentic
//!   session still compacts.
//! - Older turns are Summarized (folded into the summary).
//! - preserve_recent_tokens caps the verbatim tail's token estimate; when the
//!   tail exceeds the budget the oldest verbatim turns fold into the summary
//!   until it fits (0 disables the ceiling).
//!
//! The plan is per turn group, not per event. One group = one API round
//! (an API-round boundary fires at each new assistant response, integral
//! same fate). thinking and its tool_use blocks always land in the same
//! group, so a plan cannot split them — the API rejects a tool_use whose
//! thinking block landed in a different fate. Type First encoding makes the
//! illegal split unexpressable, not repaired later.
//!
//! Referenced is not a Compress disposition. A large tool_result is
//! externalized to the CAS at the Isolate stage (PostToolUse, before
//! Compress), so by the time build_manifest runs the result event already
//! carries a small block_ref marker. The round is Verbatim or Summarized
//! and the marker rides along; the pair stays integral. The
//! large_output_bytes knob is the Isolate threshold, reserved here until
//! the Isolate stage lands.
//!
//! The Summarizer trait is the seam: HeuristicSummarizer returns a placeholder
//! string (no LLM dependency); LlmSummarizer (in lifecycle.rs) calls a provider
//! with chunked input to prevent summarizer-self-overflow, falling back to the
//! heuristic when the provider is unavailable.

use std::time::{SystemTime, UNIX_EPOCH};

use houyicoder_async::PFut;
use houyicoder_context::{CheckpointId, CheckpointManifest, Disposition, TurnEvent, TurnEventKind};

/// Knobs for the compaction disposition policy.
#[derive(Debug, Clone)]
pub struct CompressPolicy {
    /// Number of recent assistant turns (API rounds) kept verbatim in the
    /// served view. The unit is the assistant response, not the user prompt.
    pub tail_turns: usize,
    /// Token-budget ceiling on the verbatim tail. 0 disables the ceiling so
    /// tail_turns alone governs the boundary.
    pub preserve_recent_tokens: usize,
    /// ToolResult outputs whose serialized byte length exceeds this become a
    /// CAS block_ref at the Isolate stage (PostToolUse, before Compress). This
    /// is the Isolate threshold, not a Compress disposition: build_manifest
    /// assigns only Verbatim or Summarized. Reserved here until the Isolate
    /// stage lands; 0 disables the large-output externalization path.
    pub large_output_bytes: usize,
}

impl Default for CompressPolicy {
    fn default() -> Self {
        Self {
            tail_turns: 4,
            preserve_recent_tokens: 0,
            large_output_bytes: 8192,
        }
    }
}

/// Error returned when summarization fails. The compress caller falls back to
/// the heuristic summarizer on LlmFailed so the compaction pipeline never bricks.
#[derive(Debug)]
pub enum SummarizeError {
    /// The LLM call failed (provider error, network, etc.).
    LlmFailed(String),
    /// No events to summarize (empty folded span).
    Empty,
}

impl std::fmt::Display for SummarizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LlmFailed(msg) => write!(f, "summarizer LLM call failed: {msg}"),
            Self::Empty => write!(f, "no events to summarize"),
        }
    }
}

impl std::error::Error for SummarizeError {}

/// Produces a summary string for the Summarized span. The real implementation
/// (LlmSummarizer in lifecycle.rs) calls a provider with chunked input; this
/// trait is the seam so build_manifest's callers can swap summarizers without
/// touching the disposition logic.
pub trait Summarizer: Send + Sync + std::any::Any {
    /// Summarize the given events (the folded span) into a single string.
    /// custom_instructions, when present, are merged into the summarizer
    /// prompt so a PreCompact hook (or a future /compact argument) can steer
    /// the summary. Heuristic summarizers ignore it. The lifetime ties the
    /// future to both the self borrow and the events slice so the
    /// implementation can choose to borrow or clone.
    fn summarize<'a>(
        &'a self,
        events: &'a [TurnEvent],
        custom_instructions: Option<&'a str>,
    ) -> PFut<'a, Result<String, SummarizeError>>;

    /// Type introspection for wiring verification (the composition root
    /// asserts the production runner carries an LlmSummarizer, not the
    /// heuristic placeholder). Each concrete impl returns self; the method
    /// is required (no default) because the self-to-Any upcast needs a
    /// Sized Self, which holds inside each impl but not on the trait object.
    fn as_any(&self) -> &dyn std::any::Any;
}

/// Placeholder summarizer: returns a deterministic one-line note counting the
/// folded events. No LLM dependency — exists so build_manifest can populate the
/// manifest's summary field without a provider, and as the fallback when the
/// LLM summarizer fails or no provider is wired.
pub struct HeuristicSummarizer;

impl Summarizer for HeuristicSummarizer {
    fn summarize<'a>(
        &'a self,
        events: &'a [TurnEvent],
        _custom_instructions: Option<&'a str>,
    ) -> PFut<'a, Result<String, SummarizeError>> {
        if events.is_empty() {
            return Box::pin(async move { Err(SummarizeError::Empty) });
        }
        let turns = events
            .iter()
            .filter(|e| matches!(e.kind, TurnEventKind::AssistantMessage { .. }))
            .count();
        let count = events.len();
        Box::pin(async move {
            Ok(format!(
                "Earlier conversation context ({turns} turns, {count} events)."
            ))
        })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// AssistantTextDelta is a streaming audit chunk subsumed by the authoritative
/// AssistantMessage at turn end. Projection skips it; the manifest skips it
/// too (no disposition, not in the folded span).
fn is_delta(kind: &TurnEventKind) -> bool {
    matches!(kind, TurnEventKind::AssistantTextDelta { .. })
}

/// Build a CheckpointManifest over the given events per the policy. Calls the
/// summarizer to produce the summary text for the folded (Summarized) span.
/// (SystemTime::now stamps ts, so the manifest is not byte-reproducible across
/// runs — fine, the ts is a checkpoint label.)
///
/// Disposition rules:
/// 1. The last tail_turns assistant turns (API rounds — every event at or
///    after the boundary AssistantMessage) are Verbatim. The unit is the
///    assistant response, not the user prompt: a single user prompt can drive
///    dozens of assistant turns in an agentic coding session, and counting
///    user turns would leave the whole session verbatim (compaction never
///    fires). The boundary unit is the API-round grouping for the same
///    reason; this design keeps a verbatim tail rather than summarizing the
///    whole conversation.
/// 2. All events before the boundary are Summarized.
/// 3. preserve_recent_tokens (when above 0) caps the verbatim tail's token
///    estimate; the oldest verbatim turns fold into the summary until it fits.
///
/// The plan is per turn group, not per event. A new group starts at each
/// AssistantMessage (an API-round boundary) so thinking and its tool_use
/// blocks always share a group and a disposition. A tool_result that Isolate externalized to a
/// block_ref marker stays in its round's group — the marker is small and the
/// round stays integral. Referenced is not assigned here; the Isolate stage
/// (PostToolUse) does the externalization before Compress runs.
///
/// AssistantTextDelta events are streaming audit chunks subsumed by the
/// authoritative AssistantMessage at turn end; projection skips them, so the
/// manifest skips them too — they get no disposition and are not in the folded
/// span (otherwise the LLM summarizer would see duplicated text).
///
/// Pair invariant: thinking and its tool_use share a group, so the pair is
/// structurally integral — a debug_assert guards that every event lands in
/// exactly one group and every group is non-empty.
pub async fn build_manifest(
    events: &[TurnEvent],
    policy: &CompressPolicy,
    summarizer: &dyn Summarizer,
    custom_instructions: Option<&str>,
) -> CheckpointManifest {
    let session = events.first().map(|e| e.session).unwrap_or_default();
    let last_event = events.last().map(|e| e.id).unwrap_or_default();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // 1. Compute the verbatim/summarized boundary. The token estimate uses
    //    the same tokenizer as the served view so the preserve_recent_tokens
    //    ceiling and the served-view count never disagree on what a token is.
    let tokenizer = super::context::Tokenizer::new();
    let mut boundary = verbatim_boundary(events, policy.tail_turns);
    if policy.preserve_recent_tokens > 0 {
        boundary = apply_token_ceiling(events, boundary, policy.preserve_recent_tokens, &tokenizer);
    }

    // 2. Per-event dispositions: Verbatim at/after the boundary, Summarized
    //    before. Only Verbatim or Summarized — Referenced is an Isolate
    //    concern, not a Compress disposition.
    let dispositions: Vec<Disposition> = (0..events.len())
        .map(|i| {
            if i >= boundary {
                Disposition::Verbatim
            } else {
                Disposition::Summarized
            }
        })
        .collect();

    // 3. Group events into TurnGroups. A new group starts at each
    //    AssistantMessage (an API-round boundary) so thinking and its
    //    tool_use never split. Deltas are excluded (no disposition).
    let plan = group_into_turn_groups(events, &dispositions);

    // 4. Summary of the folded (Summarized) span (deltas excluded so the
    //    LLM summarizer does not see duplicated text). SkillBody is also
    //    excluded: an invoked skill's body is revived post-compact, not
    //    summarized, so it never enters the summary channel (no untrusted-
    //    text leak, no double-billing when the revive re-serves it).
    let folded: Vec<&TurnEvent> = events
        .iter()
        .zip(dispositions.iter())
        .filter(|(e, d)| {
            !is_delta(&e.kind)
                && **d == Disposition::Summarized
                && !matches!(e.kind, TurnEventKind::SkillBody { .. })
        })
        .map(|(e, _)| e)
        .collect();
    let summary = if folded.is_empty() {
        None
    } else {
        let folded_owned: Vec<TurnEvent> = folded.into_iter().cloned().collect();
        match summarizer
            .summarize(&folded_owned, custom_instructions)
            .await
        {
            Ok(text) => Some(text),
            Err(SummarizeError::Empty) => None,
            Err(SummarizeError::LlmFailed(_)) => {
                // Fall back to the heuristic so the pipeline never bricks.
                HeuristicSummarizer
                    .summarize(&folded_owned, custom_instructions)
                    .await
                    .ok()
            }
        }
    };

    // 5. Structural guard: every non-delta event lands in exactly one group,
    //    and every group is non-empty. The grouping construction guarantees
    //    this; the assert catches a regression that would orphan an event
    //    (no disposition) or double-count one.
    debug_assert!(
        groups_cover_all(events, &plan),
        "manifest groups do not cover every non-delta event"
    );

    CheckpointManifest {
        id: CheckpointId::new(),
        session,
        last_event,
        summary,
        plan,
        ts,
    }
}

/// Group events into TurnGroups. A new group starts at each AssistantMessage
/// (an API-round boundary) and
/// when the disposition changes, so thinking and its tool_use blocks always
/// land in the same group. AssistantTextDelta events are skipped (no
/// disposition). The turn_id is the AssistantMessage id when the group
/// contains one, else the first event's id (a leading user prompt or a bare
/// tool_result).
fn group_into_turn_groups(
    events: &[TurnEvent],
    dispositions: &[Disposition],
) -> Vec<houyicoder_context::TurnGroup> {
    use houyicoder_context::TurnGroup;
    let mut groups: Vec<TurnGroup> = Vec::new();
    let mut current_ids: Vec<houyicoder_context::EventId> = Vec::new();
    let mut current_disp: Option<Disposition> = None;
    let mut current_turn_id: Option<houyicoder_context::EventId> = None;

    for (i, e) in events.iter().enumerate() {
        if is_delta(&e.kind) {
            continue;
        }
        let disp = dispositions[i];
        let is_asst = matches!(e.kind, TurnEventKind::AssistantMessage { .. });
        // A new group starts at each AssistantMessage (a new API round) and
        // when the disposition changes. The assistant boundary keeps one
        // round's thinking + tool_use in one group; the disposition boundary
        // keeps Summarized and Verbatim spans apart.
        let start_new = current_disp.is_some()
            && (current_disp != Some(disp) || (is_asst && !current_ids.is_empty()));
        if start_new {
            groups.push(TurnGroup {
                turn_id: current_turn_id.unwrap_or(current_ids[0]),
                disposition: current_disp.unwrap(),
                event_ids: std::mem::take(&mut current_ids),
            });
            current_turn_id = None;
        }
        current_ids.push(e.id);
        current_disp = Some(disp);
        if is_asst {
            current_turn_id = Some(e.id);
        }
    }
    if !current_ids.is_empty() {
        groups.push(houyicoder_context::TurnGroup {
            turn_id: current_turn_id.unwrap_or(current_ids[0]),
            disposition: current_disp.unwrap(),
            event_ids: current_ids,
        });
    }
    groups
}

/// Verify every non-delta event lands in exactly one group, and every group
/// is non-empty. The structural pair invariant (thinking + tool_use share a
/// group) falls out of the grouping; this guard catches a regression that
/// would orphan an event (no disposition applied) or duplicate one.
fn groups_cover_all(events: &[TurnEvent], plan: &[houyicoder_context::TurnGroup]) -> bool {
    use std::collections::HashSet;
    let mut seen: HashSet<houyicoder_context::EventId> = HashSet::new();
    for g in plan {
        if g.event_ids.is_empty() {
            return false;
        }
        for id in &g.event_ids {
            if !seen.insert(*id) {
                return false; // duplicated across groups
            }
        }
    }
    // Every non-delta event must appear in some group.
    events.iter().all(|e| {
        if is_delta(&e.kind) {
            return true;
        }
        seen.contains(&e.id)
    })
}

/// Index of the first verbatim event: the AssistantMessage that starts the
/// tail_turns-th-from-last assistant turn (API round). events[boundary..] is
/// the verbatim tail. Returns events.len() when tail_turns is 0 (nothing
/// verbatim) and 0 when there are fewer than tail_turns assistant turns (keep
/// all verbatim). The unit is the assistant response, not the user prompt: a
/// single user prompt can drive dozens of assistant turns in an agentic
/// session, so counting user turns would leave the whole session verbatim.
fn verbatim_boundary(events: &[TurnEvent], tail_turns: usize) -> usize {
    if events.is_empty() || tail_turns == 0 {
        return if events.is_empty() { 0 } else { events.len() };
    }
    let mut count = 0usize;
    for i in (0..events.len()).rev() {
        if matches!(events[i].kind, TurnEventKind::AssistantMessage { .. }) {
            count += 1;
            if count == tail_turns {
                return i;
            }
        }
    }
    // Fewer assistant turns than tail_turns: nothing to fold, keep all.
    0
}

/// Shrink the verbatim tail (advance the boundary forward past assistant
/// turns) until its token estimate fits the ceiling. If the tail never fits,
/// everything is Summarized (boundary reaches events.len()).
fn apply_token_ceiling(
    events: &[TurnEvent],
    mut boundary: usize,
    ceiling: usize,
    tokenizer: &super::context::Tokenizer,
) -> usize {
    while boundary < events.len() {
        let tokens = estimate_tokens(&events[boundary..], tokenizer);
        if tokens <= ceiling {
            return boundary;
        }
        // Advance past the assistant turn that currently starts the verbatim tail.
        let next = (boundary + 1..events.len())
            .find(|&i| matches!(events[i].kind, TurnEventKind::AssistantMessage { .. }));
        match next {
            Some(i) => boundary = i,
            None => return events.len(),
        }
    }
    boundary
}

/// Best-effort token estimate for an arbitrary event span, constructing the
/// shared tokenizer once. Used by the compaction path to capture pre/post
/// compact token counts for the PreCompact/PostCompact payloads + the wire
/// reply. AssistantTextDelta is skipped (same double-count rationale as
/// estimate_tokens).
pub(crate) fn estimate_span_tokens(events: &[TurnEvent]) -> usize {
    let tokenizer = super::context::Tokenizer::new();
    estimate_tokens(events, &tokenizer)
}

/// Token estimate for a span, using the same tiktoken BPE the served view
/// uses (Tokenizer::new picks the real BPE in production, the char/4 fast
/// path under the test flag — so the estimate and the served-view count
/// share one tokenizer, never two). AssistantTextDelta is skipped — it is
/// not in the served view (projection subsumes it into the
/// AssistantMessage), so counting it would double-count the assistant text.
fn estimate_tokens(events: &[TurnEvent], tokenizer: &super::context::Tokenizer) -> usize {
    events
        .iter()
        .map(|e| estimate_event_tokens(e, tokenizer))
        .sum()
}

/// Token estimate for one event. Each text-bearing field is counted with
/// the shared tokenizer; JSON tool inputs/outputs are counted as their
/// serialized form (the form the served view carries). Reasoning is
/// counted here even though projection skips it — the estimate gauges the
/// raw span the model would see if not compressed, and reasoning is part
/// of that. The three counts (served, estimate, cache key) stay separate
/// by design: served excludes reasoning, estimate includes it, cache key
/// excludes it (the system prompt is byte-stable, reasoning is in the
/// message stream).
fn estimate_event_tokens(event: &TurnEvent, tokenizer: &super::context::Tokenizer) -> usize {
    let count = |s: &str| tokenizer.count(s) as usize;
    match &event.kind {
        TurnEventKind::UserInput { text }
        | TurnEventKind::MetaUser { text }
        | TurnEventKind::MidTurnInput { text }
        | TurnEventKind::MemoryRecall { text, .. }
        | TurnEventKind::SkillListing { text, .. } => count(text),
        TurnEventKind::SkillBody { content, .. } => count(content),
        TurnEventKind::RewardObservation { .. } => 0,
        TurnEventKind::Unknown => 0,
        TurnEventKind::AssistantMessage { text, thinking } => {
            count(text) + thinking.as_ref().map(|t| count(t)).unwrap_or(0)
        }
        TurnEventKind::AssistantTextDelta { .. } => 0,
        TurnEventKind::ToolCall { input, .. } => count(&input.to_string()),
        TurnEventKind::ToolResult { output, .. } => count(&output.to_string()),
        TurnEventKind::Reasoning { text } => count(text),
        TurnEventKind::CompactionBoundary { .. } => 0,
        TurnEventKind::CacheBreak { .. } => 0,
        TurnEventKind::Summary { text } => count(text),
        TurnEventKind::PermissionDecision { .. } => 0,
        TurnEventKind::TurnAborted { reason } => count(reason),
        TurnEventKind::TruncationVerdict { .. } => 0,
        TurnEventKind::WorktreeEnter { .. } | TurnEventKind::WorktreeExit { .. } => 0,
        TurnEventKind::TurnUsage { .. }
        | TurnEventKind::HookSignal { .. }
        | TurnEventKind::TurnStarted { .. }
        | TurnEventKind::SubagentSpawn { .. }
        | TurnEventKind::SubagentReturn { .. }
        | TurnEventKind::NotificationInjected { .. } => 0,
    }
}

#[cfg(test)]
#[path = "manifest_tests.rs"]
mod tests;
