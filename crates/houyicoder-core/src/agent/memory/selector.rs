//! Recall selection: classify the lexical rank, resolve the semantic
//! verdict, and materialize the chosen bodies under the injection budget.
//!
//! The lexical rank is deterministic and cheap, so a strong lexical signal
//! injects without any model call. A weak signal (zero hits, low confidence,
//! or a tie at the selection boundary) hands the candidate metadata to the
//! semantic reranker; every reranker failure degrades to the lexical answer
//! with a typed reason, never to a recency-only guess.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use houyicoder_api::memory::{MemoryProvider, MemoryReranker, RerankOutcome};
use houyicoder_context::{MemoryEntry, MemoryRankHit, SessionId};
use tokio::task::JoinHandle;

/// Maximum keys one recall selection injects.
pub(crate) const RECALL_SELECT_CAP: usize = 5;

/// The top lexical score that counts as a confident selection. Below it the
/// semantic stage decides, because a single shared token is weak evidence.
pub(crate) const MIN_CONFIDENT_SCORE: u32 = 2;

/// Candidate rows handed to the reranker. The rank scans frontmatter only,
/// so the bound keeps the selection prompt off unbounded store growth.
pub(crate) const RERANK_CANDIDATE_CAP: usize = 50;

/// Wall-clock bound on the semantic selection call. A missed deadline is a
/// typed timeout the host falls back from, never a hung recall.
pub(crate) const RERANK_TIMEOUT: Duration = Duration::from_secs(8);

/// What the lexical rank says about a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecallVerdict {
    /// The rank returned no candidates, from an empty store or a no-signal
    /// query.
    NoCandidates,
    /// The top score is strong and the selection boundary is unambiguous.
    Confident,
    /// Candidates exist but no query token matched any of them.
    ZeroHits,
    /// The top score is below the confidence floor.
    LowConfidence,
    /// Equally scored candidates straddle the selection boundary, so the
    /// cut is arbitrary without a semantic reading.
    CandidateConflict,
}

impl RecallVerdict {
    /// Whether the semantic stage must decide this selection.
    pub(crate) fn needs_rerank(self) -> bool {
        matches!(
            self,
            Self::ZeroHits | Self::LowConfidence | Self::CandidateConflict
        )
    }
}

/// Classify a ranked candidate list. The list arrives score-descending from
/// the provider's rank.
pub(crate) fn classify(scored: &[MemoryRankHit]) -> RecallVerdict {
    if scored.is_empty() {
        return RecallVerdict::NoCandidates;
    }
    if !scored.iter().any(|h| h.score > 0) {
        return RecallVerdict::ZeroHits;
    }
    if scored[0].score < MIN_CONFIDENT_SCORE {
        return RecallVerdict::LowConfidence;
    }
    // A tie only matters at the cut: rows below the boundary with a zero
    // score are padding for the semantic stage, not competing candidates.
    if scored.len() > RECALL_SELECT_CAP
        && scored[RECALL_SELECT_CAP].score > 0
        && scored[RECALL_SELECT_CAP - 1].score == scored[RECALL_SELECT_CAP].score
    {
        return RecallVerdict::CandidateConflict;
    }
    RecallVerdict::Confident
}

/// Why a selection fell back to the lexical answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RecallFallback {
    /// No reranker is installed, so the weak lexical signal is the best
    /// answer.
    NoReranker,
    /// Neither stage found any signal; nothing is injected and nothing is
    /// guessed from recency.
    NoSignal,
    /// The session already carries the memory byte cap, so this recall
    /// ranked nothing.
    ByteCap,
    /// The reranker missed the deadline.
    Timeout,
    /// The reranker call failed.
    Unavailable(String),
    /// The reranker replied outside the key-array contract.
    Malformed(String),
}

/// Map a rerank outcome to the keys to inject plus the fallback reason when
/// the semantic verdict could not be honored. An empty Selected list is the
/// model's confident nothing-is-relevant answer and is returned as-is: it
/// must never re-open the door to the lexical rows the model just rejected.
pub(crate) fn resolve(
    outcome: RerankOutcome,
    scored: &[MemoryRankHit],
) -> (Vec<String>, Option<RecallFallback>) {
    match outcome {
        RerankOutcome::Selected(keys) => (keys, None),
        RerankOutcome::Timeout => (lexical_keys(scored), Some(RecallFallback::Timeout)),
        RerankOutcome::Unavailable(reason) => (
            lexical_keys(scored),
            Some(RecallFallback::Unavailable(reason)),
        ),
        RerankOutcome::Malformed(reason) => (
            lexical_keys(scored),
            Some(RecallFallback::Malformed(reason)),
        ),
    }
}

/// The deterministic lexical selection: matching rows in rank order, capped.
/// Zero-score rows never enter — recency alone is not evidence of relevance.
pub(crate) fn lexical_keys(scored: &[MemoryRankHit]) -> Vec<String> {
    scored
        .iter()
        .filter(|h| h.score > 0)
        .take(RECALL_SELECT_CAP)
        .map(|h| h.key.clone())
        .collect()
}

/// Read the selected bodies and pack them under the token budget. The walk
/// stops at the first entry that does not fit so the injection stays one
/// contiguous prefix of the selection; a later, smaller entry never jumps
/// the queue ahead of a rejected larger one.
pub(crate) fn materialize(
    provider: &dyn MemoryProvider,
    keys: &[String],
    budget_tokens: usize,
) -> Vec<MemoryEntry> {
    let mut out: Vec<MemoryEntry> = Vec::new();
    let mut used = 0usize;
    for key in keys {
        // A key deleted between the rank and the read is skipped, not fatal.
        let Some(entry) = provider.show_memory(key) else {
            continue;
        };
        if used + entry.tokens > budget_tokens {
            break;
        }
        used += entry.tokens;
        out.push(entry);
    }
    out
}

/// Run the semantic selection under a wall-clock bound. A missed deadline
/// yields the typed timeout outcome; the caller falls back deterministically.
pub(crate) async fn rerank_with_timeout(
    reranker: Arc<dyn MemoryReranker>,
    query: String,
    candidates: Vec<MemoryRankHit>,
    limit: usize,
    timeout: Duration,
) -> RerankOutcome {
    match tokio::time::timeout(timeout, reranker.rerank(&query, &candidates, limit)).await {
        Ok(outcome) => outcome,
        Err(_) => RerankOutcome::Timeout,
    }
}

/// Run the whole semantic stage over a ranked candidate list: bound the
/// candidates, await the selection under the deadline, and resolve the
/// verdict to the keys to inject plus a typed fallback.
pub(crate) async fn run_semantic_selection(
    reranker: Arc<dyn MemoryReranker>,
    query: String,
    scored: &[MemoryRankHit],
) -> (Vec<String>, Option<RecallFallback>) {
    let candidates: Vec<MemoryRankHit> =
        scored.iter().take(RERANK_CANDIDATE_CAP).cloned().collect();
    let outcome = rerank_with_timeout(
        reranker,
        query,
        candidates,
        RECALL_SELECT_CAP,
        RERANK_TIMEOUT,
    )
    .await;
    resolve(outcome, scored)
}

/// Per-recall telemetry for the tracing log. The selected keys and byte
/// count land durably in the recall event; the full record never reaches
/// the transcript, because the model sees the injected memories, not the
/// numbers about how they were chosen.
pub(crate) struct RecallTelemetry {
    pub(crate) candidate_count: usize,
    pub(crate) lexical_hits: usize,
    pub(crate) rerank_triggered: bool,
    pub(crate) selected_keys: Vec<String>,
    pub(crate) fallback: Option<RecallFallback>,
    pub(crate) injected_bytes: usize,
}

impl RecallTelemetry {
    pub(crate) fn log(&self) {
        tracing::info!(
            candidates = self.candidate_count,
            lexical_hits = self.lexical_hits,
            rerank = self.rerank_triggered,
            selected = ?self.selected_keys,
            fallback = ?self.fallback,
            bytes = self.injected_bytes,
            "memory recall settled"
        );
    }
}

/// Background recall selection tasks. A triggered rerank runs alongside the
/// main model call and appends its recall on completion, so the first token
/// never waits on the semantic stage. While a selection runs, its candidate
/// keys stay reserved for the session so a second recall in the same turn
/// treats them as surfaced instead of spawning a duplicate selection. The
/// drain gives the appends a bounded window at shutdown.
#[derive(Default)]
pub(crate) struct RecallTasks {
    running: Mutex<Vec<TrackedRecall>>,
}

/// One running selection: the handle plus the session and candidate keys it
/// was spawned for.
struct TrackedRecall {
    handle: JoinHandle<()>,
    session: SessionId,
    candidates: HashSet<String>,
}

impl RecallTasks {
    /// Track one spawned selection task, reserving its candidate keys for
    /// the session and reaping finished entries so the list does not grow
    /// across a long session.
    pub(crate) fn track(
        &self,
        handle: JoinHandle<()>,
        session: SessionId,
        candidates: HashSet<String>,
    ) {
        let mut running = self.running.lock().expect("recall tasks");
        running.retain(|t| !t.handle.is_finished());
        running.push(TrackedRecall {
            handle,
            session,
            candidates,
        });
    }

    /// Candidate keys reserved by still-running selections for one session.
    /// A finished selection needs no reservation because its append is in
    /// the projection, so the surfaced scan takes over from there.
    pub(crate) fn pending_keys(&self, session: SessionId) -> HashSet<String> {
        self.running
            .lock()
            .expect("recall tasks")
            .iter()
            .filter(|t| t.session == session && !t.handle.is_finished())
            .flat_map(|t| t.candidates.iter().cloned())
            .collect()
    }

    /// Await running selection tasks until they finish or the timeout
    /// expires; the rest detach and the runtime aborts them at shutdown.
    /// Taking the list releases every reservation at once, so drain must
    /// run where no recall for the same session can interleave, such as a
    /// turn join or shutdown.
    pub(crate) async fn drain(&self, timeout: Duration) {
        let tracked: Vec<TrackedRecall> =
            std::mem::take(&mut *self.running.lock().expect("recall tasks"));
        if tracked.is_empty() {
            return;
        }
        let mut deadline = Box::pin(tokio::time::sleep(timeout));
        for entry in tracked {
            tokio::select! {
                _ = entry.handle => {}
                _ = &mut deadline => return,
            }
        }
    }
}

#[cfg(test)]
mod tests {
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
            fn rerank(
                &self,
                _q: &str,
                _c: &[MemoryRankHit],
                _limit: usize,
            ) -> PFut<'_, RerankOutcome> {
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
            fn rerank(
                &self,
                _q: &str,
                _c: &[MemoryRankHit],
                _limit: usize,
            ) -> PFut<'_, RerankOutcome> {
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
}
