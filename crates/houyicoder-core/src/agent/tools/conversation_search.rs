//! The conversation recall tool. The model calls it to recall detail that was
//! folded out of the assembled context by a compaction, without re-injecting the
//! whole block. The tool replays the raw session log (append-only, never
//! mutated by a compaction), filters to the text-bearing events, and either
//! substring-searches a query or slices a turn range, returning short snippets
//! the model reads in place of the full folded span.
//!
//! A compaction folds older turns into a summary (Summarized disposition) and
//! keeps a verbatim tail (Verbatim). The raw events stay in the log; the
//! assembled context applies the manifest's disposition plan on top. So a replay
//! returns every event, including the folded ones — this tool searches that
//! full set. When a match lands in the Summarized span (the compacted detail
//! the assembled context no longer shows), the tool bumps a recall meter the
//! compaction path snapshots to compute a recall rate: of the events a
//! compaction folded, how many the model later pulled back. The rate is an
//! instrumentation signal, not a correctness gate.
//!
//! Three modes: a keyword query (case-insensitive substring, up to 10
//! matches with surrounding context), a turns range {start, end} (retrieve
//! events by index), or stats (event/folded counts). Long texts truncate so
//! the model reads snippets, not a whole re-injected folded block.

use std::borrow::Cow;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::agent::model_window::span_for_lowercase_offsets;
use houyicoder_api::session::SessionLog;
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::PFut;
use houyicoder_context::{Disposition, EventId, SessionEvent, SessionId, SessionLogEntry};
use houyicoder_protocol::extension::ToolError;
use serde::Deserialize;
use serde_json::{Value, json};

/// The conversation recall tool. Holds a shared session-log handle (to replay
/// the raw log + read the current manifest) and a shared recall meter the
/// compaction path snapshots. Both are Arc so the tool shares one instance
/// with the runner across the session.
pub struct ConversationSearchTool {
    store: Arc<dyn SessionLog>,
    recall_meter: Arc<AtomicU32>,
}

impl ConversationSearchTool {
    /// Construct with a shared session-log handle + a recall meter the
    /// compaction path snapshots. The composition root passes the same store
    /// the runner holds + the same meter the compaction path reads.
    pub fn new(store: Arc<dyn SessionLog>, recall_meter: Arc<AtomicU32>) -> Self {
        Self {
            store,
            recall_meter,
        }
    }
}

#[derive(Debug, Deserialize)]
struct SearchInput {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    turns: Option<TurnRange>,
    #[serde(default)]
    stats: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct TurnRange {
    start: usize,
    end: usize,
}

impl Tool for ConversationSearchTool {
    fn name(&self) -> &str {
        "conversation_search"
    }

    fn description(&self) -> &str {
        "Search the full conversation history (including details a compaction \
         folded out of the live view) by keyword, or retrieve a range of \
         turns. Use this to recall compacted detail without re-reading the \
         whole transcript. Pass a query for substring search, or a turns \
         range {start, end} to retrieve events by index, or stats: true for \
         conversation statistics."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Substring search query (case-insensitive). Returns up to 10 matches with surrounding context."
                },
                "turns": {
                    "type": "object",
                    "properties": {
                        "start": {"type": "integer", "description": "Start index (inclusive)."},
                        "end": {"type": "integer", "description": "End index (exclusive)."}
                    },
                    "required": ["start", "end"],
                    "description": "Retrieve text-bearing events by index range."
                },
                "stats": {
                    "type": "boolean",
                    "description": "Return conversation statistics (event count, folded count, has summary)."
                }
            },
            "additionalProperties": false
        })
    }

    fn execute(&self, ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let store = Arc::clone(&self.store);
        let recall_meter = Arc::clone(&self.recall_meter);
        Box::pin(async move {
            let params: SearchInput = serde_json::from_value(input)
                .map_err(|e| ToolError::InvalidInput(format!("conversation_search: {e}")))?;
            let session = ctx.session_id.ok_or_else(|| {
                ToolError::Failed(
                    "conversation_search: no session bound to this dispatch".to_string(),
                )
            })?;
            let events = store
                .replay(session)
                .await
                .map_err(|e| ToolError::Failed(format!("conversation_search: replay: {e}")))?;
            let folded_ids = folded_event_ids(&store, session).await;
            let text_events: Vec<&SessionLogEntry> = events
                .iter()
                .filter(|e| transcript_item(e).is_some())
                .collect();
            let mut output = String::new();

            if params.stats == Some(true) {
                output.push_str(&format_stats(&events, &text_events, &folded_ids));
            }

            // A blank query is no query: an empty substring matches every
            // text, so the search would report every event as a hit and count
            // the whole folded span as recalled.
            let query = params.query.filter(|q| !q.trim().is_empty());
            if let Some(query) = query {
                let matches = search_events(&text_events, &query);
                let folded_matches = matches
                    .iter()
                    .filter(|m| folded_ids.contains(&m.event_id))
                    .count();
                if folded_matches > 0 {
                    recall_meter.fetch_add(folded_matches as u32, Ordering::Relaxed);
                }
                output.push_str(&format_search_results(&query, &matches, folded_matches));
            }

            if let Some(range) = params.turns {
                output.push_str(&format_turn_range(&text_events, range));
            }

            if output.is_empty() {
                output.push_str(
                    "Provide a query (string) to search, a turns range \
                     {start, end} to retrieve events, or stats: true for \
                     conversation statistics.",
                );
            }
            Ok(json!({ "result": output }))
        })
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn is_destructive(&self) -> bool {
        false
    }
    fn requires_approval(&self) -> bool {
        false
    }
}

/// One keyword match: the event index (into the text-bearing events), the
/// event id (to test membership in the folded span), a role label, and a
/// context snippet around the hit.
struct SearchMatch {
    index: usize,
    event_id: EventId,
    role: &'static str,
    snippet: String,
}

/// Collect the event ids the current manifest marks Summarized (the folded
/// span the assembled context no longer shows verbatim). Empty when no compaction
/// has run. The tool uses this to count how many keyword matches landed in
/// compacted detail — the recall signal.
async fn folded_event_ids(store: &Arc<dyn SessionLog>, session: SessionId) -> Vec<EventId> {
    let Ok(view) = store.current_view(session).await else {
        return Vec::new();
    };
    let Some(manifest) = view.manifest.as_ref() else {
        return Vec::new();
    };
    manifest
        .plan
        .iter()
        .filter(|g| g.disposition == Disposition::Summarized)
        .flat_map(|g| g.event_ids.iter().cloned())
        .collect()
}

/// The label and text one event contributes to the search surface: the label
/// its hit or turn row renders under, and the text the search reads. Both come
/// from one exhaustive match, so a label can never disagree with the text it
/// names, and an event added later cannot reach the surface under a wildcard
/// label with nothing behind it. The text is borrowed where the event already
/// holds it, and built for the two that always compose one: a tool call
/// renders its input and a tool result renders its output. An assistant
/// message builds one only when it joins non-empty thinking.
///
/// None for events with nothing to recall. Deltas are subsumed by the
/// authoritative message; usage, boundaries, permission, worktree, and turn
/// markers are run bookkeeping. Child-completion notices, subagent return
/// summaries, and hook reasons also carry text, and are left out on purpose:
/// they are notices about the run rather than turns of the conversation.
fn transcript_item(event: &SessionLogEntry) -> Option<(&'static str, Cow<'_, str>)> {
    let (role, text): (&'static str, Cow<'_, str>) = match &event.event {
        SessionEvent::UserInput { text }
        | SessionEvent::MidTurnInput { text, .. }
        | SessionEvent::MetaUser { text } => ("User", Cow::Borrowed(text.as_str())),
        SessionEvent::MemoryRecall { text, .. } => ("Memory", Cow::Borrowed(text.as_str())),
        SessionEvent::SkillListing { text, .. } => ("Skill", Cow::Borrowed(text.as_str())),
        SessionEvent::SkillBody { content, .. } => ("Skill", Cow::Borrowed(content.as_str())),
        SessionEvent::AssistantMessage { text, thinking } => (
            "Assistant",
            match thinking.as_deref().filter(|t| !t.is_empty()) {
                Some(t) if text.is_empty() => Cow::Borrowed(t),
                Some(t) => Cow::Owned(format!("{text}\n{t}")),
                None => Cow::Borrowed(text.as_str()),
            },
        ),
        SessionEvent::ToolCall { tool, input, .. } => {
            ("Assistant", Cow::Owned(format!("[{tool}]\n{input}")))
        }
        SessionEvent::ToolResult { output, .. } => ("Tool", Cow::Owned(output.to_string())),
        SessionEvent::Reasoning { text } => ("Reasoning", Cow::Borrowed(text.as_str())),
        SessionEvent::Summary { text } => ("Summary", Cow::Borrowed(text.as_str())),
        SessionEvent::RewardObservation { .. }
        | SessionEvent::Unknown
        | SessionEvent::AssistantTextDelta { .. }
        | SessionEvent::CompactionBoundary { .. }
        | SessionEvent::CacheBreak { .. }
        | SessionEvent::PermissionDecision { .. }
        | SessionEvent::TurnStarted { .. }
        | SessionEvent::TurnUsage { .. }
        | SessionEvent::HookSignal { .. }
        | SessionEvent::TurnAborted { .. }
        | SessionEvent::TruncationVerdict { .. }
        | SessionEvent::WorktreeEnter { .. }
        | SessionEvent::WorktreeExit { .. }
        | SessionEvent::SubagentSpawn { .. }
        | SessionEvent::SubagentReturn { .. }
        | SessionEvent::ChildDelegated { .. }
        | SessionEvent::RunCompleted { .. }
        | SessionEvent::NotificationInjected { .. } => return None,
    };
    if text.is_empty() {
        None
    } else {
        Some((role, text))
    }
}

/// Case-insensitive substring search over the text-bearing events. Each hit
/// records its index, event id, role, and a snippet around the first match.
fn search_events(events: &[&SessionLogEntry], query: &str) -> Vec<SearchMatch> {
    let query_lower = query.to_lowercase();
    let mut results = Vec::new();
    for (idx, event) in events.iter().enumerate() {
        let Some((role, text)) = transcript_item(event) else {
            continue;
        };
        let Some(snippet) = snippet_window(&text, &query_lower) else {
            continue;
        };
        results.push(SearchMatch {
            index: idx,
            event_id: event.id,
            role,
            snippet,
        });
    }
    results
}

/// A snippet around the first hit of the query, with ellipsis when the match
/// is not at the text boundary. None when the text does not contain the query,
/// so the hit and its window are decided from one lowercase copy rather than
/// two that could disagree. The offsets come from that copy, so they are
/// mapped back to the text being cut, and the window edges are pulled to a
/// char boundary, because an offset from the copy or a fixed byte window
/// lands inside a multi-byte char often enough to be the common case.
fn snippet_window(text: &str, query_lower: &str) -> Option<String> {
    let lower = text.to_lowercase();
    let pos = lower.find(query_lower)?;
    let (hit_start, hit_end) = span_for_lowercase_offsets(text, pos, pos + query_lower.len());
    let start = text.floor_char_boundary(hit_start.saturating_sub(50));
    let end = text.ceil_char_boundary((hit_end + 50).min(text.len()));
    let mut snippet = text[start..end].to_string();
    if start > 0 {
        snippet = format!("...{snippet}");
    }
    if end < text.len() {
        snippet = format!("{snippet}...");
    }
    Some(snippet)
}

/// Render up to 10 search matches, with a tail count when truncated. The
/// folded-matches count surfaces how many hits landed in compacted detail.
fn format_search_results(query: &str, matches: &[SearchMatch], folded_matches: usize) -> String {
    if matches.is_empty() {
        return format!("## Search Results\n\nNo results found for '{query}'.\n");
    }
    let mut out = format!(
        "## Search Results for '{query}'\n\nFound {} matches",
        matches.len()
    );
    if folded_matches > 0 {
        out.push_str(&format!(" ({folded_matches} in compacted detail)"));
    }
    out.push_str(":\n\n");
    for m in matches.iter().take(10) {
        out.push_str(&format!("**[{}] {}:**\n{}\n\n", m.index, m.role, m.snippet));
    }
    if matches.len() > 10 {
        out.push_str(&format!("... and {} more results\n", matches.len() - 10));
    }
    out
}

/// Render a turn range: the text-bearing events with index in [start, end).
/// Long texts truncate to 1000 chars so the model does not re-ingest a whole
/// folded block (the whole point of recall over re-injection).
fn format_turn_range(events: &[&SessionLogEntry], range: TurnRange) -> String {
    let end = range.end.min(events.len());
    if range.start >= end {
        return format!(
            "## Turns {}-{}\n\nNo events in that range.\n",
            range.start, range.end
        );
    }
    let mut out = format!("## Turns {}-{}\n\n", range.start, range.end);
    for (i, event) in events[range.start..end].iter().enumerate() {
        let idx = range.start + i;
        // An event with no transcript text renders nothing at all, not a
        // heading over an empty body. The caller indexes text-bearing events,
        // so this only fires for a direct caller handing over a raw slice.
        let Some((role, text)) = transcript_item(event) else {
            continue;
        };
        out.push_str(&format!("**[{idx}] {role}:**\n"));
        if text.len() > 1000 {
            out.push_str(&text.chars().take(1000).collect::<String>());
            out.push_str("... (truncated)\n");
        } else {
            out.push_str(&text);
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

/// Render conversation statistics: total events, text-bearing events, folded
/// count, and whether a summary exists.
fn format_stats(
    events: &[SessionLogEntry],
    text_events: &[&SessionLogEntry],
    folded_ids: &[EventId],
) -> String {
    let has_summary = events
        .iter()
        .any(|e| matches!(e.event, SessionEvent::Summary { .. }));
    format!(
        "## Conversation Stats\n\n\
         - Total events: {}\n\
         - Text-bearing events: {}\n\
         - Folded (compacted) events: {}\n\
         - Has summary: {}\n\n",
        events.len(),
        text_events.len(),
        folded_ids.len(),
        has_summary
    )
}

#[cfg(test)]
#[path = "conversation_search_tests.rs"]
mod tests;
