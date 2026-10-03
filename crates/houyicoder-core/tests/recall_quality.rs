//! Deterministic recall quality gates over the two-hundred-entry corpus.
//! The lexical categories must land the expected key inside the top five
//! ranked rows, and the weak categories must rank zero everywhere so the
//! semantic stage is the only path that can answer them. The exact latency
//! bound and the live semantic gates run in the standalone benchmark binary.

#[path = "recall_corpus.rs"]
mod recall_corpus;

use std::collections::HashSet;
use std::time::Instant;

use houyicoder_api::memory::MemoryProvider;
use houyicoder_context::MemoryRankHit;
use recall_corpus::{CorpusPair, discard_root, load_corpus, seed_corpus};

/// The injection cap the selection boundary mirrors.
const SELECT_CAP: usize = 5;
/// The confident-lexical floor the verdict mirror uses.
const MIN_CONFIDENT: u32 = 2;
/// The candidate window the semantic stage receives.
const RERANK_WINDOW: usize = 50;

fn ranked(provider: &impl MemoryProvider, query: &str) -> Vec<MemoryRankHit> {
    provider.rank_candidates(query, &HashSet::new())
}

/// The selector's verdict, mirrored here so the report can say how many
/// lexical pairs settle without a model call. Kept in step by contract.
fn verdict_of(rows: &[MemoryRankHit]) -> &'static str {
    if rows.is_empty() {
        return "none";
    }
    if !rows.iter().any(|r| r.score > 0) {
        return "zero-hits";
    }
    if rows[0].score < MIN_CONFIDENT {
        return "low-confidence";
    }
    if rows.len() > SELECT_CAP
        && rows[SELECT_CAP].score > 0
        && rows[SELECT_CAP - 1].score == rows[SELECT_CAP].score
    {
        return "boundary-conflict";
    }
    "confident"
}

fn top_five_hits(provider: &impl MemoryProvider, pairs: &[&CorpusPair]) -> usize {
    pairs
        .iter()
        .filter(|p| {
            let expected = p.expected.as_deref().expect("lexical pair has a key");
            ranked(provider, &p.query)
                .iter()
                .take(SELECT_CAP)
                .any(|r| r.key == expected)
        })
        .count()
}

#[test]
fn test_lexical_recall_top_five() {
    let corpus = load_corpus();
    let (provider, root) = seed_corpus(&corpus);
    let en = corpus.pairs_in("en_lexical");
    let cjk = corpus.pairs_in("cjk_lexical");

    let en_hits = top_five_hits(&provider, &en);
    let cjk_hits = top_five_hits(&provider, &cjk);
    let en_ratio = en_hits as f64 / en.len() as f64;
    let cjk_ratio = cjk_hits as f64 / cjk.len() as f64;

    let mut confident = 0;
    let mut conflict = 0;
    for pair in en.iter().chain(cjk.iter()) {
        match verdict_of(&ranked(&provider, &pair.query)) {
            "confident" => confident += 1,
            "boundary-conflict" => conflict += 1,
            other => panic!("lexical pair {:?} ranked as {other}", pair.query),
        }
    }
    println!(
        "lexical recall@5: english {en_hits}/{} = {en_ratio:.3}, cjk {cjk_hits}/{} = {cjk_ratio:.3}, verdicts confident={confident} boundary-conflict={conflict}",
        en.len(),
        cjk.len()
    );
    discard_root(&root);
    assert!(
        en_ratio >= 0.95,
        "english lexical recall {en_ratio:.3} under gate"
    );
    assert!(
        cjk_ratio >= 0.85,
        "cjk lexical recall {cjk_ratio:.3} under gate"
    );
}

#[test]
fn test_weak_pairs_no_signal() {
    let corpus = load_corpus();
    let (provider, root) = seed_corpus(&corpus);
    for category in ["cjk_cross", "semantic", "negative"] {
        for pair in corpus.pairs_in(category) {
            let rows = ranked(&provider, &pair.query);
            assert_eq!(
                rows.len(),
                corpus.entries.len(),
                "every entry stays a candidate"
            );
            if let Some(row) = rows.iter().find(|r| r.score > 0) {
                panic!(
                    "{category} query {:?} lexically hit {} at score {}",
                    pair.query, row.key, row.score
                );
            }
            // With every score zero the rank falls back to recency, so the
            // expected entry must sit inside the window the semantic stage
            // receives or the pair is unanswerable by construction.
            if let Some(expected) = pair.expected.as_deref() {
                let position = rows
                    .iter()
                    .position(|r| r.key == expected)
                    .expect("expected entry ranked");
                assert!(
                    position < RERANK_WINDOW,
                    "{category} query {:?} expects {expected} at window position {position}",
                    pair.query
                );
            }
        }
    }
    discard_root(&root);
}

#[test]
fn test_rank_latency_sanity() {
    let corpus = load_corpus();
    let (provider, root) = seed_corpus(&corpus);
    let queries: Vec<&str> = corpus.pairs.iter().map(|p| p.query.as_str()).collect();
    let mut samples: Vec<u128> = Vec::new();
    // Warm the page cache, then time two full passes over every query.
    for query in queries.iter() {
        let _rows = ranked(&provider, query);
    }
    for _ in 0..2 {
        for query in queries.iter() {
            let start = Instant::now();
            let _rows = ranked(&provider, query);
            samples.push(start.elapsed().as_millis());
        }
    }
    samples.sort_unstable();
    let p95 = samples[samples.len() * 95 / 100];
    println!("rank latency over {} samples: p95 {p95}ms", samples.len());
    discard_root(&root);
    // Margin for loaded machines; the exact bound runs in the benchmark
    // binary on a quiet one.
    assert!(p95 < 50, "rank p95 {p95}ms over the sanity bound");
}
