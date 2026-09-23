//! Tests for the conversation recall tool: the label and text each event
//! contributes to the search surface, the index space both views share, the
//! recall meter, and the statistics view.

use super::*;
use houyicoder_api::session::TrajectoryHead;
use houyicoder_context::{
    CheckpointManifest, ContextBackend, ContextError, ContextSnapshot, EventId, SessionId,
    SessionLogEntry, TurnGroup,
};
use houyicoder_protocol::extension::ToolError;
use std::sync::atomic::AtomicU32;

/// An in-memory session log the tool tests drive directly. Records events
/// in a Vec; current_view returns the manifest the test sets, so the
/// folded-id path is exercisable without a real backend.
struct InMemoryLog {
    events: std::sync::Mutex<Vec<SessionLogEntry>>,
    manifest: std::sync::Mutex<Option<CheckpointManifest>>,
}

impl InMemoryLog {
    fn new() -> Self {
        Self {
            events: std::sync::Mutex::new(Vec::new()),
            manifest: std::sync::Mutex::new(None),
        }
    }
    fn push(&self, ev: SessionLogEntry) {
        self.events.lock().unwrap().push(ev);
    }
    fn set_manifest(&self, m: CheckpointManifest) {
        *self.manifest.lock().unwrap() = Some(m);
    }
}

impl SessionLog for InMemoryLog {
    fn append(&self, event: SessionLogEntry) -> PFut<'_, Result<EventId, ContextError>> {
        let id = event.id;
        self.events.lock().unwrap().push(event);
        Box::pin(async move { Ok(id) })
    }
    fn replay(&self, _session: SessionId) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
        let events = self.events.lock().unwrap().clone();
        Box::pin(async move { Ok(events) })
    }
    fn current_view(&self, session: SessionId) -> PFut<'_, Result<ContextSnapshot, ContextError>> {
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
    fn trajectory_snapshot(&self, _session: SessionId) -> Vec<SessionLogEntry> {
        self.events.lock().unwrap().clone()
    }
    fn trajectory_head(&self, session: SessionId) -> TrajectoryHead {
        // This double keeps no summary; the store that appends maintains one.
        let _ = session;
        TrajectoryHead::default()
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

fn make_event(event: SessionEvent) -> SessionLogEntry {
    SessionLogEntry {
        id: EventId::new(),
        session: SessionId::new(),
        ts: 0,
        prev_hash: None,
        event,
    }
}

fn make_session() -> SessionId {
    SessionId::new()
}

/// This test log implements only the mirror methods, so the trait supplies
/// the body and answers with the tail of that mirror.
#[test]
fn test_default_last_id_tail() {
    let log = InMemoryLog::new();
    let s = make_session();
    assert_eq!(log.last_trajectory_id(s), None, "no events, no id");
    log.push(make_event(SessionEvent::UserInput { text: "a".into() }));
    let e2 = make_event(SessionEvent::Reasoning { text: "b".into() });
    let last = e2.id;
    log.push(e2);
    assert_eq!(
        log.last_trajectory_id(s),
        Some(last),
        "the default answers the snapshot tail"
    );
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
    events: Vec<SessionLogEntry>,
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
    let folded = make_event(SessionEvent::UserInput {
        text: "remember the migration plan".to_string(),
    });
    let folded_id = folded.id;
    let verbatim = make_event(SessionEvent::AssistantMessage {
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
    let e0 = make_event(SessionEvent::UserInput {
        text: "zero".to_string(),
    });
    let e1 = make_event(SessionEvent::UserInput {
        text: "one".to_string(),
    });
    let e2 = make_event(SessionEvent::UserInput {
        text: "two".to_string(),
    });
    let e3 = make_event(SessionEvent::UserInput {
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
    let e = make_event(SessionEvent::UserInput {
        text: "hello".to_string(),
    });
    let (tool, ctx, _meter) = harness(vec![e], None);
    let out = tool
        .execute(ctx, json!({"query": "nonexistent"}))
        .await
        .unwrap();
    assert!(out.to_string().contains("No results found"));
}

/// A blank query is no query. An empty substring matches every text, so
/// without the guard the tool would report every event as a hit and count
/// the whole folded span as recalled. Whitespace matches nothing on this
/// text, so it is the second input that holds the empty-substring rule.
#[tokio::test]
async fn test_blank_query_reports_guidance() {
    for query in ["", "   "] {
        let folded = make_event(SessionEvent::UserInput {
            text: "alpha beta".to_string(),
        });
        let folded_id = folded.id;
        let manifest = make_manifest_summarized(vec![folded_id]);
        let (tool, ctx, meter) = harness(vec![folded], Some(manifest));
        let out = tool.execute(ctx, json!({"query": query})).await.unwrap();
        let text = out.to_string();
        assert!(
            text.contains("Provide a query"),
            "guidance served for {query:?}: {text}"
        );
        assert!(
            !text.contains("Search Results"),
            "no hit reported for {query:?}: {text}"
        );
        assert_eq!(
            meter.load(Ordering::Relaxed),
            0,
            "no recall counted for {query:?}"
        );
    }
}

#[tokio::test]
async fn test_stats_reports_counts() {
    let e0 = make_event(SessionEvent::UserInput {
        text: "a".to_string(),
    });
    let e1 = make_event(SessionEvent::Summary {
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
    let folded = make_event(SessionEvent::UserInput {
        text: "folded".to_string(),
    });
    let folded_id = folded.id;
    let verbatim = make_event(SessionEvent::AssistantMessage {
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

/// An Unknown event, written by a newer binary, carries no searchable text,
/// so transcript_item answers None rather than indexing garbage.
#[test]
fn test_search_text_unknown_none() {
    let e = make_event(SessionEvent::Unknown);
    assert!(transcript_item(&e).is_none());
}

/// A skill body is an item of the conversation in its own right, so it is
/// labelled as one. The label comes from the same match as the text, so a row
/// names what the model read rather than the channel it arrived on.
#[tokio::test]
async fn test_skill_body_labelled_skill() {
    let body = make_event(SessionEvent::SkillBody {
        skill_name: "demo".to_string(),
        content: "run the drills".to_string(),
        agent_id: None,
        untrusted: false,
    });
    let (tool, ctx, _meter) = harness(vec![body], None);
    let out = tool.execute(ctx, json!({"query": "drills"})).await.unwrap();
    let text = out.to_string();
    assert!(text.contains("[0] Skill:"), "hit labelled a skill: {text}");
    assert!(!text.contains("System"), "no catch-all label: {text}");
}

/// A log holding events with no transcript text still lists turns over a
/// dense index space: the indices number the text-bearing events, and
/// every heading printed carries a body. A row numbered after a spawn or
/// return event would mean the listing had counted an event it cannot show.
#[tokio::test]
async fn test_turn_rows_dense_index() {
    let e0 = make_event(SessionEvent::UserInput {
        text: "alpha".to_string(),
    });
    let spawn = make_event(SessionEvent::SubagentSpawn {
        child_session_id: "child_1".to_string(),
        subagent_type: "explore".to_string(),
        prompt_summary: "look around".to_string(),
        isolation: "worktree".to_string(),
        policy: "auto".to_string(),
        trigger_source: "model:call_1".to_string(),
    });
    let e1 = make_event(SessionEvent::UserInput {
        text: "beta".to_string(),
    });
    let (tool, ctx, _meter) = harness(vec![e0, spawn, e1], None);
    let out = tool
        .execute(ctx, json!({"turns": {"start": 0, "end": 2}}))
        .await
        .unwrap();
    let text = out.to_string();
    assert!(text.contains("[0] User:"), "first row numbered: {text}");
    assert!(text.contains("[1] User:"), "second row dense: {text}");
    assert!(!text.contains("[2]"), "no row for the spawn: {text}");
    assert!(!text.contains("explore"), "spawn not listed: {text}");
    // A heading immediately followed by a blank line is the artifact this
    // pins: a row printed for an event that had nothing to show. The output
    // is read as JSON, so a real blank line reaches this string as the two
    // characters backslash-n repeated.
    assert!(
        !text.contains(":**\\n\\n"),
        "every heading carries a body: {text}"
    );
}

/// One table pins each text-bearing event to the label it renders under and
/// the text the search reads, so an event added later cannot quietly join the
/// surface under a borrowed label.
#[test]
fn test_transcript_item_labels() {
    let cases: Vec<(SessionEvent, (&str, &str))> = vec![
        (SessionEvent::UserInput { text: "u".into() }, ("User", "u")),
        (SessionEvent::MetaUser { text: "n".into() }, ("User", "n")),
        (
            SessionEvent::MidTurnInput {
                text: "m".into(),
                pending_input_id: None,
            },
            ("User", "m"),
        ),
        (
            SessionEvent::MemoryRecall {
                text: "r".into(),
                keys: Vec::new(),
                bytes: 1,
            },
            ("Memory", "r"),
        ),
        (
            SessionEvent::SkillListing {
                text: "l".into(),
                bytes: 1,
                content_hash: 0,
            },
            ("Skill", "l"),
        ),
        (
            SessionEvent::SkillBody {
                skill_name: "s".into(),
                content: "b".into(),
                agent_id: None,
                untrusted: false,
            },
            ("Skill", "b"),
        ),
        (
            SessionEvent::AssistantMessage {
                text: "a".into(),
                thinking: None,
            },
            ("Assistant", "a"),
        ),
        (
            SessionEvent::AssistantMessage {
                text: "a".into(),
                thinking: Some("t".into()),
            },
            ("Assistant", "a\nt"),
        ),
        (
            SessionEvent::AssistantMessage {
                text: String::new(),
                thinking: Some("t".into()),
            },
            ("Assistant", "t"),
        ),
        (
            SessionEvent::ToolCall {
                call_id: "call_1".into(),
                tool: "glob".into(),
                input: json!({"pattern": "*.rs"}),
            },
            ("Assistant", "[glob]\n{\"pattern\":\"*.rs\"}"),
        ),
        (
            SessionEvent::ToolResult {
                call_id: "call_1".into(),
                output: json!("found"),
                duration_ms: 3,
            },
            ("Tool", "\"found\""),
        ),
        (
            SessionEvent::Reasoning { text: "k".into() },
            ("Reasoning", "k"),
        ),
        (SessionEvent::Summary { text: "z".into() }, ("Summary", "z")),
    ];
    for (event, (role, text)) in cases {
        let entry = make_event(event);
        let (got_role, got_text) = transcript_item(&entry).expect("a text item");
        assert_eq!(got_role, role, "label for {text}");
        assert_eq!(got_text, text);
    }
}

/// The other side of the same table: an event with nothing to recall, and an
/// event whose text is empty, are both no item at all.
#[test]
fn test_transcript_item_empty_textless() {
    let cases = vec![
        SessionEvent::UserInput {
            text: String::new(),
        },
        SessionEvent::Unknown,
        SessionEvent::AssistantTextDelta { text: "d".into() },
        SessionEvent::TurnStarted {
            turn: 1,
            call_in_turn: 0,
        },
        SessionEvent::AssistantMessage {
            text: String::new(),
            thinking: Some(String::new()),
        },
    ];
    for event in cases {
        assert!(transcript_item(&make_event(event)).is_none(), "no item");
    }
}

/// A query the text does not contain is no snippet at all, which is what
/// makes the window one decision with the search rather than a second guess
/// after it.
#[test]
fn test_snippet_miss_is_none() {
    assert!(snippet_window("alpha beta", "gamma").is_none());
    assert_eq!(
        snippet_window("alpha beta", "beta").as_deref(),
        Some("alpha beta")
    );
}

/// A hit past a char whose lowercase form is longer than the char itself
/// still cuts a snippet out of the original: the dotted capital I maps to
/// two code points, so an offset from the lowercase copy runs ahead of the
/// text it has to address. The match is served in the case the text holds,
/// which is what makes the search case-insensitive rather than the output.
/// The stretch behind the hit is repeated ahead of it, so a window that
/// runs past the copy offset lands at the tail of the text and drops the
/// match instead of merely ending early.
#[test]
fn test_snippet_longer_mapping() {
    let text = format!("{}NEEDLE{}", "İ".repeat(60), "İ".repeat(60));
    let snippet = snippet_window(&text, "needle").expect("a hit");
    assert!(
        snippet.contains("NEEDLE"),
        "hit served as written: {snippet}"
    );
    assert!(
        snippet.starts_with("...İ"),
        "the window edge starts a whole char: {snippet}"
    );
}

/// The same mapping the other way: the Kelvin sign is three bytes in the text
/// and one in its lowercase form, so an offset from the copy falls behind the
/// char that carries it and the window drifts off the hit.
#[test]
fn test_snippet_shorter_mapping() {
    let text = format!("{}NEEDLE{}", "\u{212A}".repeat(60), "\u{212A}".repeat(60));
    let snippet = snippet_window(&text, "needle").expect("a hit");
    assert!(
        snippet.contains("NEEDLE"),
        "hit served as written: {snippet}"
    );
    assert!(
        snippet.starts_with("...\u{212A}"),
        "the window edge starts a whole char: {snippet}"
    );
}

/// A hit at the head of a text that runs on in wide characters cuts its
/// trailing edge at a char boundary rather than through a multi-byte char,
/// and marks the cut.
#[test]
fn test_snippet_wide_tail() {
    let text = format!("needle{}", "中".repeat(60));
    let snippet = snippet_window(&text, "needle").expect("a hit");
    assert!(
        snippet.starts_with("needle中"),
        "the window edge ends a whole char: {snippet}"
    );
    assert!(
        snippet.ends_with("..."),
        "the cut tail is marked: {snippet}"
    );
}

/// A hit with wide characters on both sides is served whole. A wide char
/// occupies more bytes in the copy than the one budgeted per char, so a
/// walk that counts characters rather than bytes reaches the copy offset
/// long after the hit and cuts its window from the tail of the text.
#[test]
fn test_snippet_wide_both_sides() {
    let text = format!("{}needle{}", "中".repeat(60), "中".repeat(60));
    let snippet = snippet_window(&text, "needle").expect("a hit");
    assert!(snippet.contains("needle"), "hit served: {snippet}");
    assert!(
        snippet.starts_with("...中"),
        "the window edge starts a whole char: {snippet}"
    );
}

/// The turn listing over a slice that still holds untranslatable events: a
/// text-less event prints no heading, and a body past the cap truncates
/// instead of re-injecting the whole folded block it came from.
#[test]
fn test_turn_listing_raw_slice() {
    let prompt = make_event(SessionEvent::UserInput {
        text: "alpha".to_string(),
    });
    let spawn = make_event(SessionEvent::SubagentSpawn {
        child_session_id: "child_1".to_string(),
        subagent_type: "explore".to_string(),
        prompt_summary: "look around".to_string(),
        isolation: "worktree".to_string(),
        policy: "auto".to_string(),
        trigger_source: "model:call_1".to_string(),
    });
    let long = make_event(SessionEvent::Summary {
        text: "s".repeat(1200),
    });
    let events: Vec<&SessionLogEntry> = vec![&prompt, &spawn, &long];
    let out = format_turn_range(&events, TurnRange { start: 0, end: 3 });
    assert!(out.contains("**[0] User:**"), "{out}");
    assert!(!out.contains("**[1]"), "spawn prints no row: {out}");
    assert!(out.contains("... (truncated)"), "long body caps: {out}");
    assert!(!out.contains(&"s".repeat(1200)), "whole body absent: {out}");
}

/// An untrusted skill body reaches the recall output framed by the same
/// wrapper the assembled context gives it: the model reads one shape of text
/// whichever path serves it. A query into the body's own words returns the
/// snippet inside the frame, so the marker travels with every hit.
#[tokio::test]
async fn test_untrusted_body_recall_framed() {
    let body = make_event(SessionEvent::SkillBody {
        skill_name: "demo".to_string(),
        content: "run the drills".to_string(),
        agent_id: None,
        untrusted: true,
    });
    let (tool, ctx, _meter) = harness(vec![body], None);
    let out = tool.execute(ctx, json!({"query": "drills"})).await.unwrap();
    let text = out.to_string();
    assert!(
        text.contains("<untrusted_skill"),
        "the recall output frames the untrusted body: {text}"
    );
    assert!(
        text.contains("</untrusted_skill>"),
        "the frame closes after the body: {text}"
    );
    assert!(
        text.contains("run the drills"),
        "the hit still shows the matched words inside the frame: {text}"
    );
}
