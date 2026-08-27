//! The conversation recall tool. The model calls it to recall detail that was
//! folded out of the served view by a compaction, without re-injecting the
//! whole block. The tool replays the raw session log (append-only, never
//! mutated by a compaction), filters to the text-bearing events, and either
//! substring-searches a query or slices a turn range, returning short snippets
//! the model reads in place of the full folded span.
//!
//! A compaction folds older turns into a summary (Summarized disposition) and
//! keeps a verbatim tail (Verbatim). The raw events stay in the log; the
//! served view applies the manifest's disposition plan on top. So a replay
//! returns every event, including the folded ones — this tool searches that
//! full set. When a match lands in the Summarized span (the compacted detail
//! the served view no longer shows), the tool bumps a recall meter the
//! compaction path snapshots to compute a recall rate: of the events a
//! compaction folded, how many the model later pulled back. The rate is an
//! instrumentation signal, not a correctness gate.
//!
//! Three modes: a keyword query (case-insensitive substring, up to 10
//! matches with surrounding context), a turns range {start, end} (retrieve
//! events by index), or stats (event/folded counts). Long texts truncate so
//! the model reads snippets, not a whole re-injected folded block.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use houyicoder_api::session::SessionLog;
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::PFut;
use houyicoder_context::{Disposition, EventId, SessionId, TurnEvent, TurnEventKind};
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
            let text_events: Vec<&TurnEvent> = events
                .iter()
                .filter(|e| event_search_text(e).is_some())
                .collect();
            let mut output = String::new();

            if params.stats == Some(true) {
                output.push_str(&format_stats(&events, &text_events, &folded_ids));
            }

            if let Some(query) = params.query {
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
/// span the served view no longer shows verbatim). Empty when no compaction
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

/// The searchable text for a text-bearing event kind, or None for kinds with
/// no user-facing text (deltas are subsumed by the authoritative message;
/// hook signals, usage, boundaries, permission, and turn markers carry no
/// recallable content). Tool calls serialize their input so a tool-call
/// argument is searchable; tool results serialize their output likewise.
fn event_search_text(event: &TurnEvent) -> Option<String> {
    let text = match &event.kind {
        TurnEventKind::UserInput { text } => text.clone(),
        TurnEventKind::MidTurnInput { text } => text.clone(),
        TurnEventKind::MetaUser { text } => text.clone(),
        TurnEventKind::MemoryRecall { text, .. } => text.clone(),
        TurnEventKind::SkillListing { text, .. } => text.clone(),
        TurnEventKind::RewardObservation { .. } => return None,
        TurnEventKind::Unknown => return None,
        TurnEventKind::AssistantMessage { text, thinking } => {
            let mut s = text.clone();
            if let Some(t) = thinking {
                if !s.is_empty() {
                    s.push('\n');
                }
                s.push_str(t);
            }
            s
        }
        TurnEventKind::ToolCall { tool, input, .. } => {
            format!("[{tool}]\n{}", input)
        }
        TurnEventKind::ToolResult { output, .. } => output.to_string(),
        TurnEventKind::Reasoning { text } => text.clone(),
        TurnEventKind::Summary { text } => text.clone(),
        TurnEventKind::AssistantTextDelta { .. }
        | TurnEventKind::CompactionBoundary { .. }
        | TurnEventKind::CacheBreak { .. }
        | TurnEventKind::PermissionDecision { .. }
        | TurnEventKind::TurnStarted { .. }
        | TurnEventKind::TurnUsage { .. }
        | TurnEventKind::HookSignal { .. }
        | TurnEventKind::TurnAborted { .. }
        | TurnEventKind::TruncationVerdict { .. }
        | TurnEventKind::WorktreeEnter { .. }
        | TurnEventKind::WorktreeExit { .. }
        | TurnEventKind::SubagentSpawn { .. }
        | TurnEventKind::SubagentReturn { .. }
        | TurnEventKind::NotificationInjected { .. } => return None,
    };
    if text.is_empty() { None } else { Some(text) }
}

/// A role label for an event, for rendering search hits + turn listings.
fn role_of(event: &TurnEvent) -> &'static str {
    match event.kind {
        TurnEventKind::UserInput { .. }
        | TurnEventKind::MidTurnInput { .. }
        | TurnEventKind::MetaUser { .. } => "User",
        TurnEventKind::AssistantMessage { .. } => "Assistant",
        TurnEventKind::ToolCall { .. } => "Assistant",
        TurnEventKind::ToolResult { .. } => "Tool",
        TurnEventKind::Reasoning { .. } => "Reasoning",
        TurnEventKind::Summary { .. } => "Summary",
        TurnEventKind::MemoryRecall { .. } => "Memory",
        _ => "System",
    }
}

/// Case-insensitive substring search over the text-bearing events. Each hit
/// records its index, event id, role, and a snippet around the first match.
fn search_events(events: &[&TurnEvent], query: &str) -> Vec<SearchMatch> {
    let query_lower = query.to_lowercase();
    let mut results = Vec::new();
    for (idx, event) in events.iter().enumerate() {
        let Some(text) = event_search_text(event) else {
            continue;
        };
        if text.to_lowercase().contains(&query_lower) {
            results.push(SearchMatch {
                index: idx,
                event_id: event.id,
                role: role_of(event),
                snippet: extract_snippet(&text, &query_lower),
            });
        }
    }
    results
}

/// A snippet around the first hit, with ellipsis when the match is not at the
/// text boundary.
fn extract_snippet(text: &str, query_lower: &str) -> String {
    let lower = text.to_lowercase();
    if let Some(pos) = lower.find(query_lower) {
        let start = pos.saturating_sub(50);
        let end = (pos + query_lower.len() + 50).min(text.len());
        let mut snippet = text[start..end].to_string();
        if start > 0 {
            snippet = format!("...{snippet}");
        }
        if end < text.len() {
            snippet = format!("{snippet}...");
        }
        snippet
    } else {
        text.chars().take(100).collect()
    }
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
fn format_turn_range(events: &[&TurnEvent], range: TurnRange) -> String {
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
        out.push_str(&format!("**[{}] {}:**\n", idx, role_of(event)));
        if let Some(text) = event_search_text(event) {
            if text.len() > 1000 {
                out.push_str(&text.chars().take(1000).collect::<String>());
                out.push_str("... (truncated)\n");
            } else {
                out.push_str(&text);
                out.push('\n');
            }
        }
        out.push('\n');
    }
    out
}

/// Render conversation statistics: total events, text-bearing events, folded
/// count, and whether a summary exists.
fn format_stats(
    events: &[TurnEvent],
    text_events: &[&TurnEvent],
    folded_ids: &[EventId],
) -> String {
    let has_summary = events
        .iter()
        .any(|e| matches!(e.kind, TurnEventKind::Summary { .. }));
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
mod tests {
    use super::*;
    use houyicoder_context::{
        CheckpointManifest, ContextBackend, ContextError, ContextSnapshot, EventId, SessionId,
        TurnEvent, TurnGroup,
    };
    use houyicoder_protocol::extension::ToolError;
    use std::sync::atomic::AtomicU32;

    /// An in-memory session log the tool tests drive directly. Records events
    /// in a Vec; current_view returns the manifest the test sets, so the
    /// folded-id path is exercisable without a real backend.
    struct InMemoryLog {
        events: std::sync::Mutex<Vec<TurnEvent>>,
        manifest: std::sync::Mutex<Option<CheckpointManifest>>,
    }

    impl InMemoryLog {
        fn new() -> Self {
            Self {
                events: std::sync::Mutex::new(Vec::new()),
                manifest: std::sync::Mutex::new(None),
            }
        }
        fn push(&self, ev: TurnEvent) {
            self.events.lock().unwrap().push(ev);
        }
        fn set_manifest(&self, m: CheckpointManifest) {
            *self.manifest.lock().unwrap() = Some(m);
        }
    }

    impl SessionLog for InMemoryLog {
        fn append(&self, event: TurnEvent) -> PFut<'_, Result<EventId, ContextError>> {
            let id = event.id;
            self.events.lock().unwrap().push(event);
            Box::pin(async move { Ok(id) })
        }
        fn replay(&self, _session: SessionId) -> PFut<'_, Result<Vec<TurnEvent>, ContextError>> {
            let events = self.events.lock().unwrap().clone();
            Box::pin(async move { Ok(events) })
        }
        fn current_view(
            &self,
            session: SessionId,
        ) -> PFut<'_, Result<ContextSnapshot, ContextError>> {
            let events = self.events.lock().unwrap().clone();
            let manifest = self.manifest.lock().unwrap().clone();
            Box::pin(async move {
                Ok(ContextSnapshot {
                    session,
                    events,
                    last_checkpoint: manifest.as_ref().map(|m| m.id),
                    rewind_points: Vec::new(),
                    manifest,
                })
            })
        }
        fn trajectory_snapshot(&self, _session: SessionId) -> Vec<TurnEvent> {
            self.events.lock().unwrap().clone()
        }
        fn reset_trajectory(&self, _session: SessionId) {}
        fn write_checkpoint(
            &self,
            manifest: CheckpointManifest,
        ) -> PFut<'_, Result<houyicoder_context::CheckpointId, ContextError>> {
            let id = manifest.id;
            *self.manifest.lock().unwrap() = Some(manifest);
            Box::pin(async move { Ok(id) })
        }
        fn read_checkpoint(
            &self,
            _id: houyicoder_context::CheckpointId,
        ) -> PFut<'_, Result<CheckpointManifest, ContextError>> {
            let m = self.manifest.lock().unwrap().clone();
            Box::pin(async move { m.ok_or(ContextError::NotFound) })
        }
        fn list_checkpoints(
            &self,
            _session: SessionId,
        ) -> PFut<'_, Result<Vec<houyicoder_context::CheckpointId>, ContextError>> {
            Box::pin(async move { Ok(Vec::new()) })
        }
        fn backend(&self) -> &dyn ContextBackend {
            unreachable!("tool tests do not touch the backend")
        }
    }

    fn make_event(kind: TurnEventKind) -> TurnEvent {
        TurnEvent {
            id: EventId::new(),
            session: SessionId::new(),
            ts: 0,
            prev_hash: None,
            kind,
        }
    }

    fn make_session() -> SessionId {
        SessionId::new()
    }

    fn make_manifest_summarized(ids: Vec<EventId>) -> CheckpointManifest {
        let anchor = ids.first().copied().unwrap_or_else(EventId::new);
        let last = ids.last().copied().unwrap_or_else(EventId::new);
        CheckpointManifest {
            id: houyicoder_context::CheckpointId::new(),
            session: SessionId::new(),
            last_event: last,
            summary: Some("folded".to_string()),
            plan: vec![TurnGroup {
                turn_id: anchor,
                disposition: Disposition::Summarized,
                event_ids: ids,
            }],
            ts: 0,
        }
    }

    /// Build the tool + a ctx bound to a session, with an event log preloaded.
    fn harness(
        events: Vec<TurnEvent>,
        manifest: Option<CheckpointManifest>,
    ) -> (ConversationSearchTool, ToolCtx, Arc<AtomicU32>) {
        let log = Arc::new(InMemoryLog::new());
        for ev in events {
            log.push(ev);
        }
        if let Some(m) = manifest {
            log.set_manifest(m);
        }
        let meter = Arc::new(AtomicU32::new(0));
        let tool = ConversationSearchTool::new(log, Arc::clone(&meter));
        let ctx = ToolCtx::new("call_1").with_session(make_session());
        (tool, ctx, meter)
    }

    #[tokio::test]
    async fn test_recalls_compacted_detail_keyword() {
        // A folded UserInput + a verbatim AssistantMessage. Searching the
        // folded keyword lands a match in the Summarized span — the recall
        // meter bumps, proving the tool recalls compacted detail.
        let folded = make_event(TurnEventKind::UserInput {
            text: "remember the migration plan".to_string(),
        });
        let folded_id = folded.id;
        let verbatim = make_event(TurnEventKind::AssistantMessage {
            text: "ok".to_string(),
            thinking: None,
        });
        let manifest = make_manifest_summarized(vec![folded_id]);
        let (tool, ctx, meter) = harness(vec![folded, verbatim], Some(manifest));
        let out = tool
            .execute(ctx, json!({"query": "migration"}))
            .await
            .unwrap();
        let text = out.to_string();
        assert!(text.contains("migration plan"), "snippet present: {text}");
        assert!(
            text.contains("1 in compacted detail"),
            "folded-match count shown: {text}"
        );
        assert_eq!(meter.load(Ordering::Relaxed), 1, "recall meter bumped");
    }

    #[tokio::test]
    async fn test_no_session_returns_error() {
        // Without a session bound, the tool cannot replay — fail with a
        // clear error rather than a panic.
        let log = Arc::new(InMemoryLog::new());
        let meter = Arc::new(AtomicU32::new(0));
        let tool = ConversationSearchTool::new(log, meter);
        let ctx = ToolCtx::new("call_1"); // no with_session
        let err = tool.execute(ctx, json!({"query": "x"})).await.unwrap_err();
        match err {
            ToolError::Failed(_) => {}
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_search_filters_turn_range() {
        // A turns range returns only events in [start, end); events outside
        // the range are absent from the output.
        let e0 = make_event(TurnEventKind::UserInput {
            text: "zero".to_string(),
        });
        let e1 = make_event(TurnEventKind::UserInput {
            text: "one".to_string(),
        });
        let e2 = make_event(TurnEventKind::UserInput {
            text: "two".to_string(),
        });
        let e3 = make_event(TurnEventKind::UserInput {
            text: "three".to_string(),
        });
        let (tool, ctx, _meter) = harness(vec![e0, e1, e2, e3], None);
        let out = tool
            .execute(ctx, json!({"turns": {"start": 1, "end": 3}}))
            .await
            .unwrap();
        let text = out.to_string();
        assert!(text.contains("one"), "range includes one: {text}");
        assert!(text.contains("two"), "range includes two: {text}");
        assert!(!text.contains("zero"), "range excludes zero: {text}");
        assert!(!text.contains("three"), "range excludes three: {text}");
    }

    #[tokio::test]
    async fn test_query_reports_no_results() {
        let e = make_event(TurnEventKind::UserInput {
            text: "hello".to_string(),
        });
        let (tool, ctx, _meter) = harness(vec![e], None);
        let out = tool
            .execute(ctx, json!({"query": "nonexistent"}))
            .await
            .unwrap();
        assert!(out.to_string().contains("No results found"));
    }

    #[tokio::test]
    async fn test_stats_reports_counts() {
        let e0 = make_event(TurnEventKind::UserInput {
            text: "a".to_string(),
        });
        let e1 = make_event(TurnEventKind::Summary {
            text: "sum".to_string(),
        });
        let folded_id = e0.id;
        let manifest = make_manifest_summarized(vec![folded_id]);
        let (tool, ctx, _meter) = harness(vec![e0, e1], Some(manifest));
        let out = tool.execute(ctx, json!({"stats": true})).await.unwrap();
        let text = out.to_string();
        assert!(text.contains("Total events: 2"), "{text}");
        assert!(text.contains("Folded (compacted) events: 1"), "{text}");
        assert!(text.contains("Has summary: true"), "{text}");
    }

    #[tokio::test]
    async fn test_verbatim_match_skips_meter() {
        // A match in the verbatim (non-folded) span is not a recall of
        // compacted detail — the meter must stay zero.
        let folded = make_event(TurnEventKind::UserInput {
            text: "folded".to_string(),
        });
        let folded_id = folded.id;
        let verbatim = make_event(TurnEventKind::AssistantMessage {
            text: "verbatim gem".to_string(),
            thinking: None,
        });
        let manifest = make_manifest_summarized(vec![folded_id]);
        let (tool, ctx, meter) = harness(vec![folded, verbatim], Some(manifest));
        let out = tool.execute(ctx, json!({"query": "gem"})).await.unwrap();
        assert!(out.to_string().contains("verbatim gem"));
        assert_eq!(
            meter.load(Ordering::Relaxed),
            0,
            "verbatim match not a recall"
        );
    }

    /// An Unknown kind (a future-binary event type) carries no searchable
    /// text, so event_search_text returns None rather than indexing garbage.
    #[test]
    fn test_search_text_unknown_none() {
        let e = make_event(TurnEventKind::Unknown);
        assert!(event_search_text(&e).is_none());
    }
}
