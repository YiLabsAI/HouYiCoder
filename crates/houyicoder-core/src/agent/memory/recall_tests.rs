//! Recall selection, surfaced scan, and injection tests.

use super::*;
use houyicoder_api::memory::RerankOutcome;
use houyicoder_async::PFut;
use houyicoder_context::{
    Disposition, EventId, MemoryEntry, MemoryError, MemoryScope, MemorySource, SessionId,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

fn ev(session: SessionId, id: EventId, event: SessionEvent) -> SessionLogEntry {
    SessionLogEntry {
        id,
        session,
        ts: 0,
        prev_hash: None,
        event,
    }
}

fn recall(keys: &[&str]) -> SessionEvent {
    let text = "<system-reminder>...</system-reminder>";
    SessionEvent::MemoryRecall {
        text: text.into(),
        keys: keys.iter().map(|s| s.to_string()).collect(),
        bytes: text.len() as u32,
    }
}

fn user(text: &str) -> SessionEvent {
    SessionEvent::UserInput { text: text.into() }
}

fn assistant(text: &str) -> SessionEvent {
    SessionEvent::AssistantMessage {
        text: text.into(),
        thinking: None,
    }
}

fn ids(n: usize) -> Vec<EventId> {
    (0..n).map(|_| EventId::new()).collect()
}

#[test]
fn test_scan_collects_all() {
    let s = SessionId::new();
    let ids = ids(3);
    let events = vec![
        ev(s, ids[0], recall(&["alpha"])),
        ev(s, ids[1], recall(&["bravo", "charlie"])),
        ev(s, ids[2], user("query")),
    ];
    let (keys, bytes) = surfaced_memory_scan(&events, None, None);
    assert!(keys.contains("alpha"));
    assert!(keys.contains("bravo"));
    assert!(keys.contains("charlie"));
    assert_eq!(keys.len(), 3);
    let one = "<system-reminder>...</system-reminder>".len();
    assert_eq!(bytes, one * 2);
}

#[test]
fn test_scan_log_falls_back() {
    let s = SessionId::new();
    let text = "<system-reminder>old log recall</system-reminder>";
    let event = SessionLogEntry {
        id: EventId::new(),
        session: s,
        ts: 0,
        prev_hash: None,
        event: SessionEvent::MemoryRecall {
            text: text.into(),
            keys: vec!["old".into()],
            bytes: 0,
        },
    };
    let (keys, bytes) = surfaced_memory_scan(&[event], None, None);
    assert!(keys.contains("old"));
    assert_eq!(
        bytes,
        text.len(),
        "old log (bytes=0) falls back to text.len()"
    );
}

#[test]
fn test_scan_excludes_folded() {
    let s = SessionId::new();
    let ids = ids(4);
    let events = vec![
        ev(s, ids[0], recall(&["folded"])),
        ev(s, ids[1], assistant("old")),
        ev(s, ids[2], assistant("boundary")),
        ev(s, ids[3], recall(&["kept"])),
    ];
    let manifest = {
        use houyicoder_context::{CheckpointId, CheckpointManifest, Disposition, TurnGroup};
        CheckpointManifest {
            id: CheckpointId::new(),
            session: s,
            last_event: ids[3],
            summary: Some("summary".into()),
            plan: vec![
                TurnGroup {
                    turn_id: ids[0],
                    disposition: Disposition::Summarized,
                    event_ids: vec![ids[0], ids[1]],
                },
                TurnGroup {
                    turn_id: ids[2],
                    disposition: Disposition::Verbatim,
                    event_ids: vec![ids[2], ids[3]],
                },
            ],
            ts: 0,
        }
    };
    let (keys, bytes) = surfaced_memory_scan(&events, Some(&manifest), None);
    assert!(
        !keys.contains("folded"),
        "a Summarized memory-recall must drop out of the surfaced set"
    );
    assert!(
        keys.contains("kept"),
        "a Verbatim memory-recall must stay in the surfaced set"
    );
    let one = "<system-reminder>...</system-reminder>".len();
    assert_eq!(bytes, one, "a folded recall's bytes drop out of the total");
}

#[tokio::test]
async fn test_planner_folds_recall() {
    use crate::agent::manifest::{CompressPolicy, HeuristicSummarizer, build_manifest};
    let s = SessionId::new();
    let ids = ids(4);
    let events = vec![
        ev(s, ids[0], recall(&["folded"])),
        ev(s, ids[1], assistant("old turn")),
        ev(s, ids[2], assistant("boundary turn")),
        ev(s, ids[3], assistant("latest turn")),
    ];
    let policy = CompressPolicy {
        tail_turns: 2,
        preserve_recent_tokens: 0,
        large_output_bytes: 0,
    };
    let manifest = build_manifest(&events, &policy, &HeuristicSummarizer, None).await;
    let disp = manifest
        .plan
        .iter()
        .find(|g| g.event_ids.contains(&ids[0]))
        .map(|g| g.disposition)
        .expect("memory-recall event must be in the plan");
    assert_eq!(
        disp,
        Disposition::Summarized,
        "an old memory-recall must take Summarized so compaction folds it"
    );
}

/// A rank stub answering one fixed row per seeded key, plus the bodies
/// the materialize walk reads back.
struct StubProvider {
    rows: Vec<MemoryRankHit>,
    bodies: Vec<MemoryEntry>,
}

impl StubProvider {
    fn new(rows: Vec<MemoryRankHit>, bodies: Vec<MemoryEntry>) -> Self {
        Self { rows, bodies }
    }
}

impl MemoryProvider for StubProvider {
    fn rank_candidates(&self, _q: &str, surfaced: &HashSet<String>) -> Vec<MemoryRankHit> {
        self.rows
            .iter()
            .filter(|h| !surfaced.contains(&h.key))
            .cloned()
            .collect()
    }
    fn show_memory(&self, key: &str) -> Option<MemoryEntry> {
        self.bodies.iter().find(|e| e.key == key).cloned()
    }
    fn add(&self, _e: MemoryEntry) -> Result<(), MemoryError> {
        Ok(())
    }
}

/// A reranker answering one canned outcome and counting its calls.
struct StubReranker {
    outcome: RerankOutcome,
    calls: Arc<AtomicUsize>,
}

impl MemoryReranker for StubReranker {
    fn rerank(&self, _q: &str, _c: &[MemoryRankHit], _limit: usize) -> PFut<'_, RerankOutcome> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let outcome = self.outcome.clone();
        Box::pin(async move { outcome })
    }
}

fn stub_reranker(outcome: RerankOutcome) -> (Arc<dyn MemoryReranker>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let reranker: Arc<dyn MemoryReranker> = Arc::new(StubReranker {
        outcome,
        calls: Arc::clone(&calls),
    });
    (reranker, calls)
}

fn call_count(calls: &AtomicUsize) -> usize {
    calls.load(Ordering::SeqCst)
}

fn hit(key: &str, score: u32) -> MemoryRankHit {
    MemoryRankHit::new(
        key,
        format!("{key} description"),
        MemorySource::Project,
        MemoryScope::Auto,
        0,
        score,
    )
}

fn body(key: &str) -> MemoryEntry {
    MemoryEntry::new(key, format!("{key} body text"), MemorySource::Project)
}

/// A session with one user query already logged, plus a fresh store.
async fn session_with_query(text: &str) -> (Arc<dyn SessionLog>, SessionId) {
    let store: Arc<dyn SessionLog> = Arc::new(houyicoder_session::SessionStore::new(Box::new(
        houyicoder_memory::InMemoryBackend::new(),
    )));
    let session = SessionId::new();
    store
        .append(new_event(
            session,
            SessionEvent::UserInput { text: text.into() },
        ))
        .await
        .expect("append user input");
    (store, session)
}

async fn has_recall(store: &Arc<dyn SessionLog>, session: SessionId) -> bool {
    let view = store.current_view(session).await.expect("view");
    view.events
        .iter()
        .any(|e| matches!(e.event, SessionEvent::MemoryRecall { .. }))
}

/// A no-space CJK query must reach the provider and produce a MemoryRecall
/// event. A whitespace word-count gate would reject it before the provider;
/// this test goes red if that gate returns.
#[tokio::test]
async fn test_cjk_query_reaches_recall() {
    let (store, session) = session_with_query("\u{90E8}\u{7F72}\u{670D}\u{52A1}").await;
    let provider: Arc<dyn MemoryProvider> = Arc::new(StubProvider::new(
        vec![hit("\u{90E8}\u{7F72}", 2)],
        vec![
            MemoryEntry::new(
                String::from("\u{90E8}\u{7F72}"),
                String::from("body"),
                MemorySource::Feedback,
            )
            .with_meta("desc", 0),
        ],
    ));
    let gates = MemoryGates::new(true, false);
    let tasks = RecallTasks::default();
    super::recall(&store, Some(&provider), None, &tasks, &gates, session)
        .await
        .expect("recall ok");
    assert!(
        has_recall(&store, session).await,
        "a no-space CJK query must reach the provider, not be dropped by a whitespace gate"
    );
}

/// A confident lexical selection injects synchronously and never spends
/// a semantic call, even with a reranker installed.
#[tokio::test]
async fn test_confident_skips_rerank() {
    let (store, session) = session_with_query("deploy gate").await;
    let provider: Arc<dyn MemoryProvider> = Arc::new(StubProvider::new(
        vec![hit("deploy-gate", 3)],
        vec![body("deploy-gate")],
    ));
    let (reranker, calls) = stub_reranker(RerankOutcome::Selected(Vec::new()));
    let gates = MemoryGates::new(true, false);
    let tasks = RecallTasks::default();
    super::recall(
        &store,
        Some(&provider),
        Some(&reranker),
        &tasks,
        &gates,
        session,
    )
    .await
    .expect("recall ok");
    assert!(has_recall(&store, session).await, "confident injects");
    assert_eq!(
        call_count(&calls),
        0,
        "a confident lexical selection must not spend a semantic call"
    );
}

/// A zero-hit rank must reach the semantic selector; this test goes red
/// if the selector stops firing when the lexical stage finds nothing.
/// The selection runs beside the model call, so the test drains the
/// tracked task before asserting the append landed.
#[tokio::test]
async fn test_zero_hits_rerank_selects() {
    let (store, session) = session_with_query("kettle whistle").await;
    let provider: Arc<dyn MemoryProvider> = Arc::new(StubProvider::new(
        vec![hit("tea-order", 0)],
        vec![body("tea-order")],
    ));
    let (reranker, calls) = stub_reranker(RerankOutcome::Selected(vec!["tea-order".into()]));
    let gates = MemoryGates::new(true, false);
    let tasks = RecallTasks::default();
    super::recall(
        &store,
        Some(&provider),
        Some(&reranker),
        &tasks,
        &gates,
        session,
    )
    .await
    .expect("recall ok");
    tasks.drain(Duration::from_secs(1)).await;
    assert_eq!(
        call_count(&calls),
        1,
        "a zero-hit rank must fire the semantic selector"
    );
    assert!(
        has_recall(&store, session).await,
        "the selected body lands as a recall append"
    );
}

/// A confident semantic empty selection injects nothing; the weak lexical
/// rows the model rejected must not sneak in through a fallback.
#[tokio::test]
async fn test_semantic_empty_no_inject() {
    let (store, session) = session_with_query("kettle whistle").await;
    let provider: Arc<dyn MemoryProvider> = Arc::new(StubProvider::new(
        vec![hit("tea-order", 1)],
        vec![body("tea-order")],
    ));
    let (reranker, _) = stub_reranker(RerankOutcome::Selected(Vec::new()));
    let gates = MemoryGates::new(true, false);
    let tasks = RecallTasks::default();
    super::recall(
        &store,
        Some(&provider),
        Some(&reranker),
        &tasks,
        &gates,
        session,
    )
    .await
    .expect("recall ok");
    tasks.drain(Duration::from_secs(1)).await;
    assert!(
        !has_recall(&store, session).await,
        "an empty semantic verdict injects nothing, not the lexical rows"
    );
}

/// Without a reranker a weak lexical signal still injects its matching
/// rows (the deterministic fallback), while a zero-hit rank injects
/// nothing rather than guessing from recency.
#[tokio::test]
async fn test_no_reranker_falls_back() {
    let (store, session) = session_with_query("deploy gate").await;
    let provider: Arc<dyn MemoryProvider> = Arc::new(StubProvider::new(
        vec![hit("deploy-gate", 1)],
        vec![body("deploy-gate")],
    ));
    let gates = MemoryGates::new(true, false);
    let tasks = RecallTasks::default();
    super::recall(&store, Some(&provider), None, &tasks, &gates, session)
        .await
        .expect("recall ok");
    assert!(
        has_recall(&store, session).await,
        "a weak lexical hit still injects without a reranker"
    );
    // Zero hits and no reranker: nothing to inject, nothing guessed.
    let (store2, session2) = session_with_query("kettle whistle").await;
    let provider2: Arc<dyn MemoryProvider> = Arc::new(StubProvider::new(
        vec![hit("tea-order", 0)],
        vec![body("tea-order")],
    ));
    super::recall(&store2, Some(&provider2), None, &tasks, &gates, session2)
        .await
        .expect("recall ok");
    assert!(
        !has_recall(&store2, session2).await,
        "a zero-hit rank without a reranker injects nothing"
    );
}

/// An empty rank (a no-signal query or an empty store) skips the
/// semantic stage entirely and appends nothing.
#[tokio::test]
async fn test_no_candidates_no_rerank() {
    let (store, session) = session_with_query("...").await;
    let provider: Arc<dyn MemoryProvider> = Arc::new(StubProvider::new(Vec::new(), Vec::new()));
    let (reranker, calls) = stub_reranker(RerankOutcome::Selected(Vec::new()));
    let gates = MemoryGates::new(true, false);
    let tasks = RecallTasks::default();
    super::recall(
        &store,
        Some(&provider),
        Some(&reranker),
        &tasks,
        &gates,
        session,
    )
    .await
    .expect("recall ok");
    assert_eq!(
        call_count(&calls),
        0,
        "no candidates means nothing to select from"
    );
    assert!(!has_recall(&store, session).await);
}

/// A reranker that stays pending until the test releases it, so a
/// selection is still running across the second recall.
struct LatchReranker {
    release: Arc<tokio::sync::Notify>,
    calls: Arc<AtomicUsize>,
    outcome: RerankOutcome,
}

impl MemoryReranker for LatchReranker {
    fn rerank(&self, _q: &str, _c: &[MemoryRankHit], _limit: usize) -> PFut<'_, RerankOutcome> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let release = Arc::clone(&self.release);
        let outcome = self.outcome.clone();
        Box::pin(async move {
            release.notified().await;
            outcome
        })
    }
}

/// A second recall in the same turn must not re-select the candidates a
/// running selection reserved; without the reservation both selections
/// append and the key is injected twice.
#[tokio::test]
async fn test_second_recall_skips_reserved() {
    let (store, session) = session_with_query("kettle whistle").await;
    let provider: Arc<dyn MemoryProvider> = Arc::new(StubProvider::new(
        vec![hit("tea-order", 0)],
        vec![body("tea-order")],
    ));
    let release = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let reranker: Arc<dyn MemoryReranker> = Arc::new(LatchReranker {
        release: Arc::clone(&release),
        calls: Arc::clone(&calls),
        outcome: RerankOutcome::Selected(vec!["tea-order".into()]),
    });
    let gates = MemoryGates::new(true, false);
    let tasks = RecallTasks::default();
    super::recall(
        &store,
        Some(&provider),
        Some(&reranker),
        &tasks,
        &gates,
        session,
    )
    .await
    .expect("first recall ok");
    super::recall(
        &store,
        Some(&provider),
        Some(&reranker),
        &tasks,
        &gates,
        session,
    )
    .await
    .expect("second recall ok");
    release.notify_one();
    tasks.drain(Duration::from_secs(1)).await;
    assert_eq!(
        call_count(&calls),
        1,
        "reserved candidates are never re-selected"
    );
    let view = store.current_view(session).await.expect("view");
    let recalls: Vec<&Vec<String>> = view
        .events
        .iter()
        .filter_map(|e| match &e.event {
            SessionEvent::MemoryRecall { keys, .. } => Some(keys),
            _ => None,
        })
        .collect();
    assert_eq!(recalls.len(), 1, "one selection appends one recall");
    assert_eq!(recalls[0], &vec!["tea-order".to_string()]);
}
