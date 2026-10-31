//! The conversation recall tool. The model calls it to recall detail a
//! compaction folded out of the assembled context, or that a tool kept beyond
//! what the model read, without re-injecting the whole block.
//!
//! The transcript modes read the durable log page by page: a keyword query
//! answers short snippets, a turn range answers bounded rows, and a match that
//! lands in the folded span bumps the recall meter the compaction path reads.
//! The results mode answers from the stored-result catalog.

use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::agent::model_window::span_for_lowercase_offsets;
use crate::agent::skill_body::frame_untrusted_body;
use houyicoder_api::result_catalog::ResultCatalog;
use houyicoder_api::session::{SessionLog, TrajectoryPosition};
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::PFut;
use houyicoder_context::{Disposition, EventId, SessionEvent, SessionId, SessionLogEntry};
use houyicoder_protocol::extension::ToolError;
use serde::Deserialize;
use serde_json::{Value, json};

use super::results::SessionResultCatalog;

mod stored;

/// Records one page read asks for while the transcript is walked.
const TRANSCRIPT_PAGE: usize = 512;

/// The most matches one query reports. Past this the output counts the rest
/// rather than holding them, so a query that matches everywhere stays bounded.
const MAX_MATCHES: usize = 64;

/// The most turn rows one range listing returns.
const MAX_TURN_ROWS: usize = 40;

/// Characters of one turn row the listing shows.
const TURN_ROW_CHARS: usize = 1000;

/// The conversation recall tool. Holds a shared session-log handle (the raw
/// log and the current manifest) and a shared recall meter the compaction path
/// reads, plus the catalog it answers stored-result requests from. The handles
/// are shared so the tool and the runner observe one log and one meter.
pub struct ConversationSearchTool {
    store: Arc<dyn SessionLog>,
    recall_meter: Arc<AtomicU32>,
    catalog: Arc<dyn ResultCatalog>,
}

impl ConversationSearchTool {
    /// Construct with a shared session-log handle and a recall meter the
    /// compaction path reads. The composition root passes the same store the
    /// runner holds and the same meter the compaction path reads. The tool
    /// folds its own index unless the host installs one with with_catalog.
    pub fn new(store: Arc<dyn SessionLog>, recall_meter: Arc<AtomicU32>) -> Self {
        Self {
            catalog: Arc::new(SessionResultCatalog::new(Arc::clone(&store))),
            store,
            recall_meter,
        }
    }

    /// Answer from an index the host also holds, so a session's stored
    /// results are folded once rather than once per reader.
    pub fn with_catalog(mut self, catalog: Arc<dyn ResultCatalog>) -> Self {
        self.catalog = catalog;
        self
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
    #[serde(default)]
    results: Option<stored::Request>,
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
        "Search the conversation history since the last clear (including \
         detail a compaction folded out of the live view) by keyword, or \
         retrieve a range of turns. A tool result that named kept output is \
         readable here too: list what was kept, read a window of one stream, \
         or search inside it. Use this to recall compacted or truncated detail \
         without re-reading the whole transcript."
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
                },
                "results": {
                    "type": "object",
                    "properties": {
                        "op": {
                            "type": "string",
                            "enum": ["list", "search", "read", "stats"],
                            "description": "Which stored-result request to run."
                        },
                        "query": {
                            "type": "string",
                            "minLength": 1,
                            "description": "Text to find when op is search. This is text inside the stored output, not the transcript query."
                        },
                        "id": {
                            "type": "string",
                            "description": "Result id, as listed, for read or for a search inside one result."
                        },
                        "tool": {
                            "type": "string",
                            "description": "Only list results of this tool."
                        },
                        "offset": {
                            "type": "integer",
                            "description": "Byte offset to start reading at."
                        },
                        "len": {
                            "type": "integer",
                            "description": "Bytes to read, at most 8000."
                        },
                        "line": {
                            "type": "integer",
                            "description": "First line to read, counting from one."
                        },
                        "count": {
                            "type": "integer",
                            "description": "Lines to read, at most 200."
                        },
                        "cursor": {
                            "type": "string",
                            "description": "The cursor a previous page returned, to continue from."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Results per page, at most 20."
                        }
                    },
                    "required": ["op"]
                }
            },
            "additionalProperties": false
        })
    }

    fn execute(&self, ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        Box::pin(async move {
            let params: SearchInput = serde_json::from_value(input)
                .map_err(|e| ToolError::InvalidInput(format!("conversation_search: {e}")))?;
            let session = ctx.session_id.ok_or_else(|| {
                ToolError::Failed(
                    "conversation_search: no session bound to this dispatch".to_string(),
                )
            })?;
            if params.results.is_some()
                && (params.query.is_some() || params.turns.is_some() || params.stats.is_some())
            {
                // The two request families answer different questions, and
                // quietly preferring one would drop the other without saying
                // so.
                return Err(ToolError::InvalidInput(
                    "conversation_search: ask for one of query, turns, stats or results, not several"
                        .to_string(),
                ));
            }
            if let Some(request) = params.results {
                let output = stored::serve(&*self.catalog, session, request, &*self.store).await?;
                return Ok(json!({ "result": output }));
            }
            if let Some(q) = params.query.as_ref()
                && q.trim().is_empty()
            {
                // An empty query is not a search with no hits: it would match
                // every text event and count the whole folded span as recalled,
                // so it is refused rather than answered.
                return Err(ToolError::InvalidInput(
                    "conversation_search: a query search needs a non-empty query".to_string(),
                ));
            }
            let folded = folded_event_ids(&self.store, session).await;
            let scan = scan_transcript(&self.store, session, params.query, params.turns, folded)?;
            if scan.folded_matches > 0 {
                self.recall_meter
                    .fetch_add(scan.folded_matches as u32, Ordering::Relaxed);
            }
            let output = scan.render(params.stats == Some(true));
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

/// What one walk of the transcript collected.
struct TranscriptScan {
    query: Option<String>,
    folded: HashSet<EventId>,
    events: usize,
    text_events: usize,
    summary: bool,
    folded_matches: usize,
    matches: Vec<SearchMatch>,
    dropped_matches: usize,
    turns: Option<TurnRange>,
    rows: Vec<TurnRow>,
    dropped_rows: usize,
    /// Log bytes the walk passed over without reading, because the record
    /// holding them was wider than one read.
    skipped_bytes: u64,
}

/// One row a turn listing shows.
struct TurnRow {
    index: usize,
    role: &'static str,
    text: String,
}

impl TranscriptScan {
    fn new(query: Option<String>, folded: HashSet<EventId>, turns: Option<TurnRange>) -> Self {
        Self {
            query,
            folded,
            events: 0,
            text_events: 0,
            summary: false,
            folded_matches: 0,
            matches: Vec::new(),
            dropped_matches: 0,
            turns,
            rows: Vec::new(),
            dropped_rows: 0,
            skipped_bytes: 0,
        }
    }

    /// Fold one log record into what the request answers with.
    fn absorb(&mut self, event: &SessionLogEntry) {
        self.events += 1;
        if matches!(event.event, SessionEvent::Summary { .. }) {
            self.summary = true;
        }
        let Some((role, text)) = transcript_item(event) else {
            return;
        };
        let index = self.text_events;
        self.text_events += 1;
        if let Some(query) = &self.query
            && let Some(snippet) = snippet_window(&text, query)
        {
            if self.folded.contains(&event.id) {
                self.folded_matches += 1;
            }
            if self.matches.len() < MAX_MATCHES {
                self.matches.push(SearchMatch {
                    index,
                    role,
                    snippet,
                });
            } else {
                self.dropped_matches += 1;
            }
        }
        if let Some(range) = &self.turns
            && index >= range.start
            && index < range.end
        {
            if self.rows.len() < MAX_TURN_ROWS {
                self.rows.push(TurnRow {
                    index,
                    role,
                    text: cut_chars(&text, TURN_ROW_CHARS),
                });
            } else {
                self.dropped_rows += 1;
            }
        }
    }

    fn render(&self, stats: bool) -> String {
        let mut output = String::new();
        if stats {
            output.push_str(&format_stats(
                self.events,
                self.text_events,
                self.folded.len(),
                self.summary,
            ));
        }
        if let Some(query) = &self.query {
            output.push_str(&format_search_results(
                query,
                &self.matches,
                self.folded_matches,
                self.dropped_matches,
                self.skipped_bytes > 0,
            ));
        }
        if let Some(range) = &self.turns {
            output.push_str(&format_turn_rows(range, &self.rows, self.dropped_rows));
        }
        if self.skipped_bytes > 0 {
            output.push_str(&format!(
                "\nThe walk passed over {} bytes of records it could not read, \
                 so this answer may be missing events.\n",
                self.skipped_bytes
            ));
        }
        if output.is_empty() {
            output.push_str(
                "Provide a query (string) to search, a turns range \
                 {start, end} to retrieve events, stats: true for \
                 conversation statistics, or results: {op} for output a \
                 tool kept beyond what you read.",
            );
        }
        output
    }
}

/// Walk the session's log page by page and collect what the request asks for.
///
/// A page is released as the next is read, so a long session costs one page of
/// memory plus the rows the answer reports, rather than a copy of the log.
fn scan_transcript(
    store: &Arc<dyn SessionLog>,
    session: SessionId,
    query: Option<String>,
    turns: Option<TurnRange>,
    folded: HashSet<EventId>,
) -> Result<TranscriptScan, ToolError> {
    // The caller guards an empty query upstream, so a None here is a request
    // that never named a query (turns or stats mode), not a blank one.
    let query = query
        .filter(|q| !q.trim().is_empty())
        .map(|q| q.to_lowercase());
    let mut scan = TranscriptScan::new(query, folded, turns);
    let mut position = TrajectoryPosition::start();
    loop {
        let page = store
            .trajectory_page(session, position, TRANSCRIPT_PAGE)
            .map_err(|e| ToolError::Failed(format!("conversation_search: read: {e}")))?;
        // A page that advances without holding a record is a record the walk
        // passed over, not the end of the log, so the walk goes on from where
        // the page stopped and remembers the bytes it could not read.
        let moved = page.next != position;
        position = page.next;
        scan.skipped_bytes += page.skipped_bytes;
        for event in &page.events {
            scan.absorb(event);
        }
        if !moved {
            break;
        }
    }
    Ok(scan)
}

/// The first limit characters of a text, with a note when it was cut.
fn cut_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit).collect();
    out.push_str("... (truncated)");
    out
}

/// One keyword match: its index among the text-bearing events, the label it
/// renders under, and a context snippet around the hit.
struct SearchMatch {
    index: usize,
    role: &'static str,
    snippet: String,
}

/// Collect the event ids the current manifest marks Summarized (the folded
/// span the assembled context no longer shows verbatim). Empty when no compaction
/// has run. The tool uses this to count how many keyword matches landed in
/// compacted detail — the recall signal.
async fn folded_event_ids(store: &Arc<dyn SessionLog>, session: SessionId) -> HashSet<EventId> {
    let Ok(view) = store.current_view(session).await else {
        return HashSet::new();
    };
    let Some(manifest) = view.manifest.as_ref() else {
        return HashSet::new();
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
/// An untrusted skill body enters framed by the same wrapper the assembled
/// context gives it, so the model reads one shape of text whichever path
/// serves it. The wrapper's words join the searchable text: a query on the
/// marker matches every untrusted body, and a query on the body's own words
/// still returns its snippet inside the frame.
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
        SessionEvent::SkillBody {
            skill_name,
            content,
            untrusted,
            ..
        } => (
            "Skill",
            if *untrusted {
                Cow::Owned(frame_untrusted_body(skill_name, content, true))
            } else {
                Cow::Borrowed(content.as_str())
            },
        ),
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
        | SessionEvent::PermissionAsked { .. }
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
        | SessionEvent::NotificationInjected { .. }
        | SessionEvent::ModelStepTiming { .. }
        | SessionEvent::CacheDecisions { .. }
        | SessionEvent::CasModelView { .. }
        | SessionEvent::CasRead { .. }
        // A recap is a summary the host wrote about the run, not a turn of
        // it; it is also excluded from model context, so recalling it as
        // conversation would feed the model its own second-hand account.
        | SessionEvent::RecapAttempt { .. }
        | SessionEvent::AwayRecap { .. }
        | SessionEvent::ContextCleared { .. } => return None,
    };
    if text.is_empty() {
        None
    } else {
        Some((role, text))
    }
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
/// folded-matches count reports how many hits landed in compacted detail.
fn format_search_results(
    query: &str,
    matches: &[SearchMatch],
    folded_matches: usize,
    dropped: usize,
    unread: bool,
) -> String {
    if matches.is_empty() && dropped == 0 {
        // A walk that passed over bytes cannot report that nothing matches;
        // it can only report what it read.
        if unread {
            return format!(
                "## Search Results\n\nNo match for '{query}' in the records the walk could read.\n"
            );
        }
        return format!("## Search Results\n\nNo results found for '{query}'.\n");
    }
    let mut out = format!(
        "## Search Results for '{query}'\n\nFound {} matches",
        matches.len() + dropped
    );
    if folded_matches > 0 {
        out.push_str(&format!(" ({folded_matches} in compacted detail)"));
    }
    out.push_str(":\n\n");
    for m in matches.iter().take(10) {
        out.push_str(&format!("**[{}] {}:**\n{}\n\n", m.index, m.role, m.snippet));
    }
    let more = matches.len().saturating_sub(10) + dropped;
    if more > 0 {
        out.push_str(&format!("... and {more} more results\n"));
    }
    out
}

/// Render the turn rows one range collected. Each row carries a body, so a
/// text-less event never reaches the listing as a heading over nothing.
fn format_turn_rows(range: &TurnRange, rows: &[TurnRow], dropped: usize) -> String {
    if rows.is_empty() && dropped == 0 {
        return format!(
            "## Turns {}-{}\n\nNo events in that range.\n",
            range.start, range.end
        );
    }
    let mut out = format!("## Turns {}-{}\n\n", range.start, range.end);
    for row in rows {
        out.push_str(&format!("**[{}] {}:**\n", row.index, row.role));
        out.push_str(&row.text);
        out.push('\n');
        out.push('\n');
    }
    if dropped > 0 {
        out.push_str(&format!("... and {dropped} more events in that range\n"));
    }
    out
}

/// Render conversation statistics: total events, text-bearing events, folded
/// count, and whether a summary exists.
fn format_stats(events: usize, text_events: usize, folded: usize, has_summary: bool) -> String {
    format!(
        "## Conversation Stats\n\n\
         - Total events: {events}\n\
         - Text-bearing events: {text_events}\n\
         - Folded (compacted) events: {folded}\n\
         - Has summary: {has_summary}\n\n"
    )
}

#[cfg(test)]
mod tests;
