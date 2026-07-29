//! Re-derivable compaction backbone + conversation recall tool wiring. Split
//! from context_wiring.rs so that file stays under the file-size gate. The
//! conversation_search tool replays the raw log to recall compacted detail +
//! bumps a recall meter; the backbone derives a structured block from the
//! folded events + workspace, merges it after the LLM summary (authoritative
//! on conflict), and measures a conflict rate.

use std::sync::Arc;

use houyicoder_api::provider::{ModelProvider, stream_from_response};
use houyicoder_api::session::SessionLog;
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::{PFut, PStream};
use houyicoder_context::{
    CheckpointId, CheckpointManifest, ContextBackend, Disposition, EventId, SessionId, TurnEvent,
    TurnEventKind, TurnGroup,
};
use houyicoder_core::agent::compact::CompactOutcome;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{
    ConversationSearchTool, Runner, SummarizeError, Summarizer, ToolRegistry,
};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, LlmEvent, ModelCapabilities, OutputItem, ProviderError,
    Usage,
};
use houyicoder_session::SessionStore;

/// A minimal canned-response provider. The compact path never calls it (the
/// FixedSummarizer handles the summary), but Runner construction requires one.
struct CannedProvider;

impl ModelProvider for CannedProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        let resp = CompletionResponse {
            output: vec![OutputItem::Text { text: "ok".into() }],
            usage: Usage::default(),
            model: "test".into(),
        };
        Box::pin(async move { Ok(resp) })
    }
    fn stream(&self, _req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        let resp = CompletionResponse {
            output: vec![OutputItem::Text { text: "ok".into() }],
            usage: Usage::default(),
            model: "test".into(),
        };
        stream_from_response(resp)
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

/// Seed a session store with a folded UserInput + a verbatim AssistantMessage,
/// plus a manifest that Summarizes the UserInput. The tool replays the full
/// log (both events) but the served view would fold the UserInput; a search
/// for the folded keyword lands a match in the Summarized span.
async fn seed_folded_session() -> (Arc<SessionStore>, SessionId) {
    let session = SessionId::new();
    let backend = InMemoryBackend::new();
    let folded = TurnEvent {
        id: EventId::new(),
        session,
        ts: 0,
        prev_hash: None,
        kind: TurnEventKind::UserInput {
            text: "remember the migration plan".into(),
        },
    };
    let verbatim = TurnEvent {
        id: EventId::new(),
        session,
        ts: 1,
        prev_hash: None,
        kind: TurnEventKind::AssistantMessage {
            text: "latest response".into(),
            thinking: None,
        },
    };
    backend.append(folded.clone()).await.unwrap();
    backend.append(verbatim.clone()).await.unwrap();
    let manifest = CheckpointManifest {
        id: CheckpointId::new(),
        session,
        last_event: verbatim.id,
        summary: Some("folded earlier turns".into()),
        plan: vec![TurnGroup {
            turn_id: folded.id,
            disposition: Disposition::Summarized,
            event_ids: vec![folded.id],
        }],
        ts: 0,
    };
    backend.write_checkpoint(manifest).await.unwrap();
    let store = Arc::new(SessionStore::new(Box::new(backend)));
    (store, session)
}

/// A shared log handle coerced from the concrete store so the tool + the
/// runner share one session log.
fn log_handle(store: &Arc<SessionStore>) -> Arc<dyn SessionLog> {
    store.clone()
}

/// The conversation recall tool searches the full raw log (including
/// compacted-out detail) and bumps the recall meter when a match lands in the
/// Summarized span. A folded event the served view no longer shows is still
/// recallable, and the recall counts toward the meter.
#[tokio::test]
async fn test_conversation_recalls_compacted_detail() {
    let (store, session) = seed_folded_session().await;
    let runner = Runner::new(
        store.clone(),
        Arc::new(CannedProvider),
        ToolRegistry::new(),
        RunnerConfig {
            model: "test".into(),
            instructions: String::new(),
            max_turns: 5,
            ..RunnerConfig::default()
        },
    );
    let meter = runner.recall_meter();
    let tool = ConversationSearchTool::new(log_handle(&store), meter.clone());
    let ctx = ToolCtx::new("call_1").with_session(session);
    let out = tool
        .execute(ctx, serde_json::json!({"query": "migration"}))
        .await
        .expect("ok");
    let text = out.to_string();
    assert!(
        text.contains("migration plan"),
        "folded detail is recalled: {text}"
    );
    assert!(
        text.contains("1 in compacted detail"),
        "folded-match count surfaces: {text}"
    );
    assert_eq!(
        meter.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "recall meter bumped for the folded match"
    );
}

/// Seed a session with more assistant turns than the default tail_turns (4)
/// so a compaction's own manifest folds the older turns. Used for the
/// recall-rate test, which needs compact to fold at least one event.
async fn seed_rich_session() -> (Arc<SessionStore>, SessionId) {
    let session = SessionId::new();
    let backend = InMemoryBackend::new();
    for i in 0..6 {
        let u = TurnEvent {
            id: EventId::new(),
            session,
            ts: i as u64 * 2,
            prev_hash: None,
            kind: TurnEventKind::UserInput {
                text: format!("prompt {i}"),
            },
        };
        let a = TurnEvent {
            id: EventId::new(),
            session,
            ts: i as u64 * 2 + 1,
            prev_hash: None,
            kind: TurnEventKind::AssistantMessage {
                text: format!("answer {i}"),
                thinking: None,
            },
        };
        backend.append(u).await.unwrap();
        backend.append(a).await.unwrap();
    }
    let store = Arc::new(SessionStore::new(Box::new(backend)));
    (store, session)
}

/// After conversation_search bumps the meter, a compaction snapshots it and
/// the CompactOutcome carries a non-None recall rate (recalls / folded
/// count). The rate is computed from the same meter the tool bumps, not a
/// separate channel; the snapshot resets the meter for the next interval.
#[tokio::test]
async fn test_recall_rate_in_report() {
    let (store, session) = seed_rich_session().await;
    let runner = Runner::new(
        store.clone(),
        Arc::new(CannedProvider),
        ToolRegistry::new(),
        RunnerConfig {
            model: "test".into(),
            instructions: String::new(),
            max_turns: 5,
            ..RunnerConfig::default()
        },
    );
    // Simulate prior recalls: bump the meter as the tool would on folded
    // matches. The compaction snapshots + resets, then normalizes by its own
    // folded count.
    runner
        .recall_meter()
        .store(2, std::sync::atomic::Ordering::Relaxed);
    let outcome: CompactOutcome = runner.compact(session).await.expect("compact");
    assert!(
        outcome.folded_count > 0,
        "fixture must fold at least one event for a rate"
    );
    let rate = outcome
        .recall_rate
        .expect("recall rate populated after meter bumps");
    let expected = 2.0 / outcome.folded_count as f64;
    assert!(
        (rate - expected).abs() < 1e-9,
        "rate {rate} == recalls 2 / folded {}: expected {expected}",
        outcome.folded_count
    );
    assert_eq!(
        runner
            .recall_meter()
            .load(std::sync::atomic::Ordering::Relaxed),
        0,
        "meter reset after snapshot"
    );
}

/// A turn range filters to events in [start, end); events outside the range
/// are absent. Pins the turn-range retrieval path.
#[tokio::test]
async fn test_search_filters_turn_range() {
    let session = SessionId::new();
    let backend = InMemoryBackend::new();
    for t in ["zero", "one", "two", "three"] {
        let ev = TurnEvent {
            id: EventId::new(),
            session,
            ts: 0,
            prev_hash: None,
            kind: TurnEventKind::UserInput { text: t.into() },
        };
        backend.append(ev).await.unwrap();
    }
    let store = Arc::new(SessionStore::new(Box::new(backend)));
    let meter = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let tool = ConversationSearchTool::new(log_handle(&store), meter);
    let ctx = ToolCtx::new("call_1").with_session(session);
    let out = tool
        .execute(ctx, serde_json::json!({"turns": {"start": 1, "end": 3}}))
        .await
        .expect("ok");
    let text = out.to_string();
    assert!(text.contains("one"), "range includes one: {text}");
    assert!(text.contains("two"), "range includes two: {text}");
    assert!(!text.contains("zero"), "range excludes zero: {text}");
    assert!(!text.contains("three"), "range excludes three: {text}");
}

// --- re-derivable compaction backbone ---

/// A summarizer returning a fixed string, so the LLM narrative is
/// deterministic and the backbone merge and conflict rate are testable
/// without a provider. The backbone path runs against whatever this returns.
struct FixedSummarizer {
    text: String,
}

impl FixedSummarizer {
    fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

impl Summarizer for FixedSummarizer {
    fn summarize<'a>(
        &'a self,
        events: &'a [TurnEvent],
        _custom: Option<&'a str>,
    ) -> PFut<'a, Result<String, SummarizeError>> {
        if events.is_empty() {
            return Box::pin(async move { Err(SummarizeError::Empty) });
        }
        let text = self.text.clone();
        Box::pin(async move { Ok(text) })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Seed a session with more assistant turns than the default tail_turns (4) so
/// a compaction folds the older turns, plus an edit tool call touching a known
/// file early enough to land in the folded (Summarized) span (the last 4
/// assistant turns stay verbatim).
async fn seed_backbone_session(file: &str) -> (Arc<SessionStore>, SessionId) {
    let session = SessionId::new();
    let backend = InMemoryBackend::new();
    // Turn 0 — the edit sits here, before the verbatim tail, so it folds.
    let u0 = TurnEvent {
        id: EventId::new(),
        session,
        ts: 0,
        prev_hash: None,
        kind: TurnEventKind::UserInput {
            text: "prompt 0".into(),
        },
    };
    let edit = TurnEvent {
        id: EventId::new(),
        session,
        ts: 1,
        prev_hash: None,
        kind: TurnEventKind::ToolCall {
            call_id: "edit1".into(),
            tool: "edit".into(),
            input: serde_json::json!({"path": file, "old_string": "x", "new_string": "y"}),
        },
    };
    let a0 = TurnEvent {
        id: EventId::new(),
        session,
        ts: 2,
        prev_hash: None,
        kind: TurnEventKind::AssistantMessage {
            text: "answer 0".into(),
            thinking: None,
        },
    };
    backend.append(u0).await.unwrap();
    backend.append(edit).await.unwrap();
    backend.append(a0).await.unwrap();
    // Turns 1..6 — the last 4 stay verbatim, turn 1 + the edit fold.
    for i in 1..6 {
        let u = TurnEvent {
            id: EventId::new(),
            session,
            ts: (i as u64 + 1) * 10,
            prev_hash: None,
            kind: TurnEventKind::UserInput {
                text: format!("prompt {i}"),
            },
        };
        let a = TurnEvent {
            id: EventId::new(),
            session,
            ts: (i as u64 + 1) * 10 + 1,
            prev_hash: None,
            kind: TurnEventKind::AssistantMessage {
                text: format!("answer {i}"),
                thinking: None,
            },
        };
        backend.append(u).await.unwrap();
        backend.append(a).await.unwrap();
    }
    let store = Arc::new(SessionStore::new(Box::new(backend)));
    (store, session)
}

fn backbone_runner(store: Arc<SessionStore>, summarizer_text: &str) -> Runner {
    Runner::new(
        store,
        Arc::new(CannedProvider),
        ToolRegistry::new(),
        RunnerConfig {
            model: "test".into(),
            instructions: String::new(),
            max_turns: 5,
            ..RunnerConfig::default()
        },
    )
    .with_summarizer(Box::new(FixedSummarizer::new(summarizer_text)))
}

/// v1 is add-only: after a compaction the committed summary carries BOTH the
/// LLM narrative AND the derived-from-log backbone block (the LLM path is not
/// shrunk, the backbone is added).
#[tokio::test]
async fn test_adds_backbone_keeps_llm() {
    let (store, session) = seed_backbone_session("src/real.rs").await;
    let runner = backbone_runner(store.clone(), "the LLM summary narrative");
    let outcome = runner.compact(session).await.expect("compact");
    let manifest = store
        .read_checkpoint(outcome.manifest_id)
        .await
        .expect("read checkpoint");
    let summary = manifest.summary.expect("summary present");
    assert!(
        summary.contains("the LLM summary narrative"),
        "LLM narrative kept (add-only): {summary}"
    );
    assert!(
        summary.contains("derived-from-log"),
        "backbone block appended: {summary}"
    );
}

/// On conflict the backbone wins: the LLM summary fabricates a touched file,
/// the backbone block lists only the real touched set, and the conflict rate
/// is non-zero. The LLM narrative keeps its fabrication (the rate measures
/// it); the authoritative backbone block corrects it.
#[tokio::test]
async fn test_backbone_wins_conflict() {
    let (store, session) = seed_backbone_session("src/real.rs").await;
    let runner = backbone_runner(store.clone(), "we edited fake_file.rs earlier");
    let outcome = runner.compact(session).await.expect("compact");
    assert!(
        outcome.conflict_rate.unwrap_or(0.0) > 0.0,
        "conflict rate non-zero for a fabricated path"
    );
    let manifest = store
        .read_checkpoint(outcome.manifest_id)
        .await
        .expect("read checkpoint");
    let summary = manifest.summary.expect("summary present");
    let block = summary
        .split("--- derived-from-log")
        .nth(1)
        .expect("backbone block present");
    assert!(
        block.contains("src/real.rs"),
        "backbone lists the real file"
    );
    assert!(
        !block.contains("fake_file.rs"),
        "backbone does not carry the fabrication"
    );
}

// --- economy gate (proactive compact) ---

/// A process-unique nonce so each test's temp cwd is distinct (parallel
/// nextest runs do not collide on the same directory).
fn unique_nonce() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// A canned provider with a small context window so the economy gate's
/// served > window/2 condition is reachable in a test without seeding a
/// 100k-token conversation. Returns a fixed final-text response so the run
/// reaches FinalOutput after the proactive compact.
struct SmallWindowProvider {
    window: u32,
}

impl ModelProvider for SmallWindowProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        let resp = CompletionResponse {
            output: vec![OutputItem::Text {
                text: "done".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        };
        Box::pin(async move { Ok(resp) })
    }
    fn stream(&self, _req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        let resp = CompletionResponse {
            output: vec![OutputItem::Text {
                text: "done".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        };
        stream_from_response(resp)
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            context_window: self.window,
            max_output_tokens: 500,
            ..Default::default()
        }
    }
}

/// The economy gate fires a proactive compact when the served view exceeds
/// half the window AND the cost decision (breakeven over the remaining turns)
/// says compact. Pins the proactive path: the run reaches FinalOutput AND a
/// CompactionBoundary event lands in the log (the compact fired before the
/// model call), without hitting the ceiling (overflow-guard) path.
#[tokio::test]
async fn test_economy_gate_fires_compact() {
    let session = SessionId::new();
    let backend = InMemoryBackend::new();
    // Seed 50 short turns so served (system ~3k + 50 turns ~8.5k = ~11.5k)
    // exceeds window/2 (10k) on turn 1's pre-flight. The compact folds 46
    // turns, keeps the last 4 (the verbatim tail ~700 tokens) so the
    // post-compact served (~3k system + summary + 700 tail = ~4.2k) sits
    // below the ceiling threshold (6500) — the run proceeds to FinalOutput
    // instead of tripping the overflow path. The window (20k) is larger than
    // the estimation margin (13k) so the ceiling threshold is non-zero.
    for i in 0..50 {
        let u = TurnEvent {
            id: EventId::new(),
            session,
            ts: i as u64 * 2,
            prev_hash: None,
            kind: TurnEventKind::UserInput {
                text: "x".repeat(400),
            },
        };
        let a = TurnEvent {
            id: EventId::new(),
            session,
            ts: i as u64 * 2 + 1,
            prev_hash: None,
            kind: TurnEventKind::AssistantMessage {
                text: format!("a{i}{}", "y".repeat(400)),
                thinking: None,
            },
        };
        backend.append(u).await.unwrap();
        backend.append(a).await.unwrap();
    }
    let store = Arc::new(SessionStore::new(Box::new(backend)));
    // Isolate the cwd so the system-prompt walk-up does not read this repo's
    // AGENTS.md (large) — otherwise the system prompt alone blows the window.
    let cwd = std::env::temp_dir().join(format!("economy-test-{}", unique_nonce()));
    drop(std::fs::create_dir_all(&cwd));
    let runner = Runner::new(
        store.clone(),
        Arc::new(SmallWindowProvider { window: 20_000 }),
        ToolRegistry::new(),
        RunnerConfig {
            model: "test".into(),
            instructions: String::new(),
            max_turns: 5,
            ..RunnerConfig::default()
        },
    )
    .with_cwd(cwd.clone());
    let result = runner.run(session, "go".into()).await.expect("run");
    assert!(
        matches!(
            result.outcome,
            houyicoder_core::agent::RunOutcome::FinalOutput(_)
        ),
        "run reaches final output after the proactive compact"
    );
    let events = store.replay(session).await.expect("replay");
    let compacted = events
        .iter()
        .any(|e| matches!(e.kind, TurnEventKind::CompactionBoundary { .. }));
    assert!(
        compacted,
        "economy gate fired a proactive compact (CompactionBoundary present)"
    );
}

/// A no-progress compact that leaves the view still over the ceiling sets a
/// Sticky suppress so the next turn does not retry pointlessly (the view
/// cannot shrink — all-Verbatim). Pins the still-over suppress path +
/// confirms the run fail-closes with ContextOverflowNoProgress.
#[tokio::test]
async fn test_still_over_sets_sticky() {
    use houyicoder_core::agent::compact::CompactSuppress;
    let session = SessionId::new();
    let backend = InMemoryBackend::new();
    // Two assistant turns — below the default tail_turns=4, so a compact
    // keeps everything Verbatim (no progress). With a small window + an
    // isolated cwd the served view sits over the ceiling threshold.
    for i in 0..2 {
        let u = TurnEvent {
            id: EventId::new(),
            session,
            ts: i as u64 * 2,
            prev_hash: None,
            kind: TurnEventKind::UserInput {
                text: "x".repeat(400),
            },
        };
        let a = TurnEvent {
            id: EventId::new(),
            session,
            ts: i as u64 * 2 + 1,
            prev_hash: None,
            kind: TurnEventKind::AssistantMessage {
                text: format!("a{i}{}", "y".repeat(400)),
                thinking: None,
            },
        };
        backend.append(u).await.unwrap();
        backend.append(a).await.unwrap();
    }
    let store = Arc::new(SessionStore::new(Box::new(backend)));
    let cwd = std::env::temp_dir().join(format!("still-over-{}", unique_nonce()));
    drop(std::fs::create_dir_all(&cwd));
    let runner = Runner::new(
        store.clone(),
        Arc::new(SmallWindowProvider { window: 4_000 }),
        ToolRegistry::new(),
        RunnerConfig {
            model: "test".into(),
            instructions: String::new(),
            max_turns: 3,
            ..RunnerConfig::default()
        },
    )
    .with_cwd(cwd);
    let result = runner.run(session, "go".into()).await;
    assert!(
        matches!(
            result.unwrap_err(),
            houyicoder_core::agent::RunError::ContextOverflowNoProgress
        ),
        "no-progress over-ceiling run fails closed"
    );
    assert_eq!(
        runner.compact_suppress(),
        CompactSuppress::Sticky,
        "still-over no-progress sets a Sticky suppress"
    );
}
