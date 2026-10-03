//! The recall benchmark proper: the exact latency bound for the
//! deterministic rank and the live semantic gates for the full recall
//! pipeline. Both tests are ignored; the live one soft-skips without a
//! credential and spends one small model call per weak-signal query while
//! the main loop stays on a canned stub.

#[path = "recall_corpus.rs"]
mod recall_corpus;

use std::collections::HashSet;
use std::env;
use std::sync::Arc;
use std::time::{Duration, Instant};

use houyicoder_api::memory::MemoryProvider;
use houyicoder_api::provider::{ModelProvider, stream_from_response};
use houyicoder_api::session::SessionLog;
use houyicoder_async::{PFut, PStream};
use houyicoder_context::{SessionEvent, SessionId};
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{MemoryGates, MemoryRuntime, Runner, ToolRegistry};
use houyicoder_memory::{InMemoryBackend, SemanticReranker};
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, LlmEvent, ModelCapabilities, OutputItem, ProviderError,
    Usage,
};
use houyicoder_provider::OpenAiCompatibleProvider;
use houyicoder_session::SessionStore;
use recall_corpus::{Corpus, CorpusPair, discard_root, load_corpus, seed_corpus};

/// Model under test, overridable so a benchmark run can pin a version.
fn test_model() -> String {
    env::var("HOUYICODER_TEST_MODEL").unwrap_or_else(|_| "qwen3.7-max".to_string())
}

/// Endpoint base for the live model calls.
fn test_base_url() -> String {
    env::var("DASHSCOPE_BASE_URL")
        .unwrap_or_else(|_| "https://dashscope.aliyuncs.com/compatible-mode/v1".to_string())
}

/// The credential the live suite needs, absent when the environment carries
/// none and the benchmark soft-skips.
fn test_api_key() -> Option<String> {
    env::var("DASHSCOPE_API_KEY").ok().filter(|k| !k.is_empty())
}

/// The main-loop model, canned so every token spent goes to the semantic
/// selection stage under measurement.
struct StubModel;

fn done_response() -> CompletionResponse {
    CompletionResponse {
        output: vec![OutputItem::Text {
            text: "done".into(),
        }],
        usage: Usage::default(),
        model: "stub".to_string(),
    }
}

impl ModelProvider for StubModel {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        Box::pin(async { Ok(done_response()) })
    }

    fn stream(&self, _req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        stream_from_response(done_response())
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

fn percentile(sorted: &[u128], pct: usize) -> u128 {
    sorted[sorted.len() * pct / 100]
}

/// Times the deterministic rank stage, the dominant cost of the fast path.
/// The synchronous settle adds the verdict read and up to five body reads,
/// which stay inside the same budget.
#[test]
#[ignore]
fn test_fast_path_latency_exact() {
    let corpus = load_corpus();
    let (provider, root) = seed_corpus(&corpus);
    let queries: Vec<&str> = corpus.pairs.iter().map(|p| p.query.as_str()).collect();
    for query in queries.iter() {
        let _rows = provider.rank_candidates(query, &HashSet::new());
    }
    let mut samples: Vec<u128> = Vec::new();
    for _ in 0..5 {
        for query in queries.iter() {
            let start = Instant::now();
            let _rows = provider.rank_candidates(query, &HashSet::new());
            samples.push(start.elapsed().as_micros());
        }
    }
    samples.sort_unstable();
    let p50 = percentile(&samples, 50);
    let p95 = percentile(&samples, 95);
    let p99 = percentile(&samples, 99);
    println!(
        "rank latency over {} files, {} samples: p50 {p50}us p95 {p95}us p99 {p99}us",
        corpus.entries.len(),
        samples.len()
    );
    discard_root(&root);
    assert!(p95 < 5000, "rank p95 {p95}us over the 5ms fast-path bound");
}

/// One recall pass over the whole corpus through the production pipeline:
/// turn start ranks, a confident lexical top settles synchronously, a weak
/// signal spawns the live semantic stage beside the canned main call, and
/// the join at turn end lands the selection as a durable recall event.
async fn injected_keys(
    runner: &Runner,
    store: &Arc<dyn SessionLog>,
    pair: &CorpusPair,
) -> Vec<String> {
    let session = SessionId::new();
    let outcome = runner.run(session, pair.query.clone()).await;
    assert!(outcome.is_ok(), "turn for {:?} must complete", pair.query);
    runner.join_dreams(Duration::from_secs(90)).await;
    let view = store.current_view(session).await.expect("session view");
    let mut keys = Vec::new();
    for entry in &view.events {
        if let SessionEvent::MemoryRecall { keys: recalled, .. } = &entry.event {
            keys.extend(recalled.iter().cloned());
        }
    }
    keys
}

fn score_pair(pair: &CorpusPair, keys: &[String]) -> bool {
    match pair.expected.as_deref() {
        Some(expected) => keys.iter().any(|k| k == expected),
        None => keys.is_empty(),
    }
}

fn ratio(hits: usize, total: usize) -> f64 {
    hits as f64 / total.max(1) as f64
}

#[tokio::test]
#[ignore]
async fn test_live_recall_gates() {
    let Some(api_key) = test_api_key() else {
        eprintln!("skip: DASHSCOPE_API_KEY unset, live recall benchmark needs a credential");
        return;
    };
    let corpus: Corpus = load_corpus();
    let (memory, root) = seed_corpus(&corpus);
    let model = Arc::new(OpenAiCompatibleProvider::new(test_base_url(), api_key));
    let reranker = Arc::new(SemanticReranker::new(model, test_model()));
    let store: Arc<dyn SessionLog> = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let mut runtime = MemoryRuntime::from_parts(
        store.clone(),
        Some(Arc::new(memory)),
        MemoryGates::new(true, true),
        None,
        None,
    );
    runtime.install_reranker(reranker);
    let runner = Runner::new(
        store.clone(),
        Arc::new(StubModel),
        ToolRegistry::new(),
        RunnerConfig::default(),
    )
    .install_memory(runtime);

    let mut misses: Vec<String> = Vec::new();
    let mut per_category: Vec<(&str, usize, usize)> = Vec::new();
    for category in [
        "en_lexical",
        "cjk_lexical",
        "cjk_cross",
        "semantic",
        "negative",
    ] {
        let pairs = corpus.pairs_in(category);
        let mut hits = 0;
        for pair in pairs.iter() {
            let keys = injected_keys(&runner, &store, pair).await;
            if score_pair(pair, &keys) {
                hits += 1;
            } else {
                misses.push(format!(
                    "{category} {:?} expected {:?} got {keys:?}",
                    pair.query, pair.expected
                ));
            }
        }
        per_category.push((category, hits, pairs.len()));
    }

    let positives = |cats: &[&str]| -> (usize, usize) {
        per_category
            .iter()
            .filter(|(c, _, _)| cats.contains(c))
            .fold((0, 0), |(h, t), (_, ph, pt)| (h + ph, t + pt))
    };
    let (all_hits, all_total) = positives(&["en_lexical", "cjk_lexical", "cjk_cross", "semantic"]);
    let (cjk_hits, cjk_total) = positives(&["cjk_lexical", "cjk_cross"]);
    let (neg_hits, neg_total) = positives(&["negative"]);
    let overall = ratio(all_hits, all_total);
    let cjk = ratio(cjk_hits, cjk_total);
    let negative = ratio(neg_hits, neg_total);

    println!("live recall benchmark, model {}", test_model());
    for (category, hits, total) in &per_category {
        println!("  {category}: {hits}/{total} = {:.3}", ratio(*hits, *total));
    }
    println!("overall recall@5 {all_hits}/{all_total} = {overall:.3} (gate 0.90)");
    println!("cjk recall@5 {cjk_hits}/{cjk_total} = {cjk:.3} (gate 0.85)");
    println!("negative precision {neg_hits}/{neg_total} = {negative:.3} (gate 0.90)");
    for miss in &misses {
        println!("  miss: {miss}");
    }
    discard_root(&root);

    assert!(overall >= 0.90, "overall recall {overall:.3} under gate");
    assert!(cjk >= 0.85, "cjk recall {cjk:.3} under gate");
    assert!(
        negative >= 0.90,
        "negative precision {negative:.3} under gate"
    );
}
