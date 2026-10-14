//! Tests for this module.

use super::*;
use houyicoder_async::PFut;
use houyicoder_context::{MemoryError, MemoryScope, MemorySource};
use std::collections::HashSet;

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

fn ranked(scores: &[u32]) -> Vec<MemoryRankHit> {
    scores
        .iter()
        .enumerate()
        .map(|(i, s)| hit(&format!("k{i}"), *s))
        .collect()
}

#[test]
fn test_classify_no_candidates() {
    assert_eq!(classify(&[]), RecallVerdict::NoCandidates);
}

#[test]
fn test_classify_zero_hits() {
    // Candidates exist for the semantic stage, but no token matched.
    assert_eq!(classify(&ranked(&[0, 0, 0])), RecallVerdict::ZeroHits);
}

#[test]
fn test_classify_low_confidence() {
    assert_eq!(classify(&ranked(&[1, 1, 0])), RecallVerdict::LowConfidence);
}

#[test]
fn test_classify_confident() {
    assert_eq!(classify(&ranked(&[3, 2, 1])), RecallVerdict::Confident);
}

/// The boundary tie only counts among matching rows: a zero-score row at
/// the cut is padding, not a competing candidate.
#[test]
fn test_classify_conflict_at_cut() {
    let tie = ranked(&[4, 3, 2, 2, 2, 2, 0]);
    assert_eq!(classify(&tie), RecallVerdict::CandidateConflict);
    let zero_pad = ranked(&[4, 3, 2, 2, 2, 0, 0]);
    assert_eq!(classify(&zero_pad), RecallVerdict::Confident);
    // Fewer rows than the cap: no cut, no conflict.
    let short = ranked(&[2, 2, 2]);
    assert_eq!(classify(&short), RecallVerdict::Confident);
}

#[test]
fn test_resolve_selected_keys() {
    let (keys, fallback) = resolve(
        RerankOutcome::Selected(vec!["k2".into(), "k0".into()]),
        &ranked(&[3, 2, 1]),
    );
    assert_eq!(keys, vec!["k2", "k0"]);
    assert_eq!(fallback, None);
}

/// A confident semantic empty selection stays empty; the lexical rows the
/// model rejected must not sneak back in.
#[test]
fn test_resolve_empty_stays_empty() {
    let (keys, fallback) = resolve(RerankOutcome::Selected(Vec::new()), &ranked(&[1, 1]));
    assert!(keys.is_empty());
    assert_eq!(fallback, None);
}

#[test]
fn test_resolve_failures_fall_back() {
    let scored = ranked(&[1, 1, 0]);
    let (keys, fallback) = resolve(RerankOutcome::Timeout, &scored);
    assert_eq!(keys, vec!["k0", "k1"]);
    assert_eq!(fallback, Some(RecallFallback::Timeout));
    let (_, fallback) = resolve(RerankOutcome::Unavailable("no route".into()), &scored);
    assert_eq!(
        fallback,
        Some(RecallFallback::Unavailable("no route".into()))
    );
    let (_, fallback) = resolve(RerankOutcome::Malformed("prose".into()), &scored);
    assert_eq!(fallback, Some(RecallFallback::Malformed("prose".into())));
}

/// A fallback over zero-hit candidates yields nothing: no recency guess.
#[test]
fn test_resolve_zero_hit_fallback() {
    let (keys, _) = resolve(RerankOutcome::Timeout, &ranked(&[0, 0]));
    assert!(keys.is_empty(), "a fallback never invents from recency");
}

#[test]
fn test_lexical_keys_caps() {
    let keys = lexical_keys(&ranked(&[5, 4, 3, 2, 1, 1, 1]));
    assert_eq!(keys.len(), RECALL_SELECT_CAP);
    assert_eq!(keys[0], "k0");
    let with_zeros = lexical_keys(&ranked(&[2, 0, 1]));
    assert_eq!(with_zeros, vec!["k0", "k2"], "zero rows never enter");
}

/// A stub store holding fixed bodies for the budget walk.
struct BodyStore {
    entries: Vec<MemoryEntry>,
}

impl MemoryProvider for BodyStore {
    fn show_memory(&self, key: &str) -> Option<MemoryEntry> {
        self.entries.iter().find(|e| e.key == key).cloned()
    }
    fn add(&self, _entry: MemoryEntry) -> Result<(), MemoryError> {
        Ok(())
    }
    fn rank_candidates(&self, _q: &str, _s: &HashSet<String>) -> Vec<MemoryRankHit> {
        Vec::new()
    }
}

fn body(key: &str, tokens: usize) -> MemoryEntry {
    let mut e = MemoryEntry::new(key, format!("{key} body"), MemorySource::Project);
    e.tokens = tokens;
    e
}

/// An entry whose token count exactly fits the budget must be included;
/// one token over must break the walk, not skip ahead to a smaller entry.
#[test]
fn test_materialize_budget_boundary() {
    let store = BodyStore {
        entries: vec![body("a", 3), body("b", 2), body("c", 1)],
    };
    let keys: Vec<String> = vec!["a".into(), "b".into(), "c".into()];
    // Budget 5: a (3) fits, b (2) fits exactly, c would overflow.
    let out = materialize(&store, &keys, 5);
    let got: Vec<&str> = out.iter().map(|e| e.key.as_str()).collect();
    assert_eq!(got, vec!["a", "b"], "an exact fit is included");
    // Budget 4: b overflows and the walk breaks — c never jumps ahead.
    let out = materialize(&store, &keys, 4);
    let got: Vec<&str> = out.iter().map(|e| e.key.as_str()).collect();
    assert_eq!(
        got,
        vec!["a"],
        "an overflow breaks the walk, not skips ahead"
    );
}

/// A key deleted between the rank and the read is skipped without
/// consuming budget or breaking the walk.
#[test]
fn test_materialize_skips_absent() {
    let store = BodyStore {
        entries: vec![body("a", 1), body("c", 1)],
    };
    let keys: Vec<String> = vec!["a".into(), "gone".into(), "c".into()];
    let out = materialize(&store, &keys, 10);
    let got: Vec<&str> = out.iter().map(|e| e.key.as_str()).collect();
    assert_eq!(got, vec!["a", "c"]);
}

/// A reranker that never answers must yield the typed timeout, not hang.
#[tokio::test]
async fn test_rerank_timeout_elapses() {
    struct Never;
    impl MemoryReranker for Never {
        fn rerank(&self, _q: &str, _c: &[MemoryRankHit], _limit: usize) -> PFut<'_, RerankOutcome> {
            Box::pin(std::future::pending())
        }
    }
    let out = rerank_with_timeout(
        Arc::new(Never),
        "query".into(),
        ranked(&[1]),
        RECALL_SELECT_CAP,
        Duration::from_millis(20),
    )
    .await;
    assert_eq!(out, RerankOutcome::Timeout);
}

/// A prompt answer inside the deadline passes through unchanged.
#[tokio::test]
async fn test_rerank_within_deadline() {
    struct Instant;
    impl MemoryReranker for Instant {
        fn rerank(&self, _q: &str, _c: &[MemoryRankHit], _limit: usize) -> PFut<'_, RerankOutcome> {
            Box::pin(async { RerankOutcome::Selected(vec!["k0".into()]) })
        }
    }
    let out = rerank_with_timeout(
        Arc::new(Instant),
        "query".into(),
        ranked(&[2]),
        RECALL_SELECT_CAP,
        Duration::from_secs(1),
    )
    .await;
    assert_eq!(out, RerankOutcome::Selected(vec!["k0".into()]));
}

/// The drain awaits a tracked task to completion, so a spawned recall
/// append lands before shutdown proceeds.
#[tokio::test]
async fn test_tasks_drain_awaits() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let done = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&done);
    let tasks = RecallTasks::default();
    tasks.track(
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            flag.store(true, Ordering::SeqCst);
        }),
        SessionId::new(),
        HashSet::new(),
    );
    tasks.drain(Duration::from_secs(1)).await;
    assert!(done.load(Ordering::SeqCst), "drain awaited the task");
    // A second drain over an emptied list is a no-op.
    tasks.drain(Duration::from_millis(10)).await;
}

/// A running selection reserves its candidate keys for its own session;
/// finishing releases them, since the surfaced scan then sees the
/// append.
#[tokio::test]
async fn test_pending_keys_reserve() {
    let session = SessionId::new();
    let tasks = RecallTasks::default();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tasks.track(
        tokio::spawn(async move {
            drop(rx.await);
        }),
        session,
        HashSet::from(["tea-order".to_string()]),
    );
    let reserved: HashSet<String> = HashSet::from(["tea-order".to_string()]);
    assert_eq!(tasks.pending_keys(session), reserved);
    assert!(
        tasks.pending_keys(SessionId::new()).is_empty(),
        "another session sees no reservation"
    );
    drop(tx);
    tasks.drain(Duration::from_secs(1)).await;
    assert!(
        tasks.pending_keys(session).is_empty(),
        "a finished selection reserves nothing"
    );
}
