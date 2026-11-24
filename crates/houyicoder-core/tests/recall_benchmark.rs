//! The recall benchmark proper: the exact latency bound for the
//! deterministic rank and the live semantic gates for the full recall
//! pipeline. Both tests are ignored; the live one soft-skips without a
//! credential and spends one small model call per weak-signal query while
//! the main loop stays on a canned stub.

#[path = "recall_corpus.rs"]
mod recall_corpus;

use std::collections::HashSet;
use std::env;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use houyicoder_api::memory::MemoryProvider;
use houyicoder_api::provider::{ModelProvider, stream_from_response};
use houyicoder_api::session::SessionLog;
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::{PFut, PStream};
use houyicoder_context::SessionId;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{MemoryGates, MemoryRuntime, Runner, ToolRegistry};
use houyicoder_memory::{InMemoryBackend, SemanticReranker};
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, InputItem, LlmEvent, ModelCapabilities, OutputItem,
    ProviderError, Usage,
};
use houyicoder_provider::OpenAiCompatibleProvider;
use houyicoder_session::SessionStore;
use recall_corpus::{Corpus, CorpusPair, discard_root, load_corpus, seed_corpus};
use serde_json::json;

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
/// selection stage under measurement. It records the text of every request
/// so the benchmark can judge what the model actually received. For queries
/// that require the asynchronous semantic stage, it issues a dummy tool call
/// on step 0 and finishes on step 1 after the background prefetch settles.
struct StubModel {
    captured: Mutex<Vec<String>>,
    two_step: bool,
}

impl StubModel {
    fn new(two_step: bool) -> Self {
        Self {
            captured: Mutex::new(Vec::new()),
            two_step,
        }
    }

    /// The text of the last request the model received. For single-step
    /// queries this is step 0; for two-step queries this is step 1.
    fn last_request(&self) -> String {
        self.captured
            .lock()
            .expect("capture lock")
            .last()
            .cloned()
            .unwrap_or_default()
    }

    /// The text of the step-0 request.
    fn step_zero(&self) -> String {
        self.captured
            .lock()
            .expect("capture lock")
            .first()
            .cloned()
            .unwrap_or_default()
    }

    fn reset(&self) {
        self.captured.lock().expect("capture lock").clear();
    }

    fn record(&self, request: &CompletionRequest) {
        self.captured
            .lock()
            .expect("capture lock")
            .push(request_text(request));
    }
}

/// Flatten a request into the text a model would read: the instructions plus
/// every input item in order.
fn request_text(request: &CompletionRequest) -> String {
    let mut text = request.instructions.clone();
    for item in &request.input {
        text.push('\n');
        match item {
            InputItem::User { content } => text.push_str(content),
            InputItem::Assistant { content, .. } => text.push_str(content),
            InputItem::ToolResult { output, .. } => text.push_str(&output.to_string()),
        }
    }
    text
}

fn done_response() -> CompletionResponse {
    CompletionResponse {
        output: vec![OutputItem::Text {
            text: "done".into(),
        }],
        usage: Usage::default(),
        model: "stub".to_string(),
    }
}

struct BenchmarkWaitTool(Arc<OnceLock<Weak<Runner>>>);

impl Tool for BenchmarkWaitTool {
    fn name(&self) -> &str {
        "wait_for_prefetch"
    }
    fn description(&self) -> &str {
        "benchmark test tool that waits for background prefetch to settle"
    }
    fn input_schema(&self) -> serde_json::Value {
        json!({ "type": "object" })
    }
    fn execute(
        &self,
        _ctx: ToolCtx,
        _input: serde_json::Value,
    ) -> PFut<'_, Result<serde_json::Value, ToolError>> {
        let runner_slot = Arc::clone(&self.0);
        Box::pin(async move {
            if let Some(runner) = runner_slot.get().and_then(Weak::upgrade) {
                runner.join_dreams(Duration::from_secs(10)).await;
            }
            Ok(json!({ "status": "waited" }))
        })
    }
}

fn wait_tool_response() -> CompletionResponse {
    CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: "call_wait".into(),
            name: "wait_for_prefetch".into(),
            input: json!({}),
        }],
        usage: Usage::default(),
        model: "stub".to_string(),
    }
}

impl ModelProvider for StubModel {
    fn complete(
        &self,
        req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        self.record(&req);
        let step = self.captured.lock().expect("capture lock").len();
        let resp = if self.two_step && step == 1 {
            wait_tool_response()
        } else {
            done_response()
        };
        Box::pin(async move { Ok(resp) })
    }

    fn stream(&self, req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        self.record(&req);
        let step = self.captured.lock().expect("capture lock").len();
        let resp = if self.two_step && step == 1 {
            wait_tool_response()
        } else {
            done_response()
        };
        stream_from_response(resp)
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

/// Run one turn and return the text of the request where recall lands.
/// For synchronous fast-path categories, this is step 0. For asynchronous
/// semantic categories, the model runs a two-step turn so the background
/// prefetch completes and lands on step 1.
async fn run_turn_for_pair(
    model: &StubModel,
    runner: &Runner,
    pair: &CorpusPair,
    is_async: bool,
) -> String {
    let session = SessionId::new();
    model.reset();
    let outcome = runner.run(session, pair.query.clone()).await;
    assert!(outcome.is_ok(), "turn for {:?} must complete", pair.query);
    if is_async {
        model.last_request()
    } else {
        model.step_zero()
    }
}

/// The body a correct recall must deliver for this pair, when it names one.
fn expected_body<'a>(corpus: &'a Corpus, pair: &CorpusPair) -> Option<&'a str> {
    let key = pair.expected.as_deref()?;
    corpus
        .entries
        .iter()
        .find(|entry| entry.key == key)
        .map(|entry| entry.body.as_str())
}

fn score_pair(pair: &CorpusPair, corpus: &Corpus, request_text: &str) -> bool {
    match pair.expected.as_deref() {
        Some(_) => expected_body(corpus, pair).is_some_and(|body| request_text.contains(body)),
        // A negative must carry no recalled block at all; the always-on index
        // names descriptions, not bodies, so its presence is not a recall.
        None => !request_text.contains("# Recalled memories"),
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
    let memory_arc: Arc<dyn MemoryProvider> = Arc::new(memory);
    let mut misses: Vec<String> = Vec::new();
    let mut per_category: Vec<(&str, usize, usize)> = Vec::new();
    for category in [
        "en_lexical",
        "cjk_lexical",
        "cjk_cross",
        "semantic",
        "negative",
    ] {
        let is_async = matches!(category, "cjk_cross" | "semantic" | "negative");
        let model = Arc::new(StubModel::new(is_async));
        let mut runtime = MemoryRuntime::from_parts(
            store.clone(),
            Some(memory_arc.clone()),
            MemoryGates::new(true, true),
            None,
            None,
        );
        runtime.install_reranker(reranker.clone());
        let mut tools = ToolRegistry::new();
        let runner_slot: Arc<OnceLock<Weak<Runner>>> = Arc::new(OnceLock::new());
        tools.register(Arc::new(BenchmarkWaitTool(Arc::clone(&runner_slot))));
        let runner = Arc::new(
            Runner::new(store.clone(), model.clone(), tools, RunnerConfig::default())
                .install_memory(runtime),
        );
        drop(runner_slot.set(Arc::downgrade(&runner)));

        let pairs = corpus.pairs_in(category);
        let mut hits = 0;
        for pair in pairs.iter() {
            let request = run_turn_for_pair(&model, &runner, pair, is_async).await;
            if is_async {
                assert!(
                    !model.step_zero().contains("# Recalled memories"),
                    "step 0 must stay non-blocking on weak-signal query {:?}",
                    pair.query
                );
            }
            if score_pair(pair, &corpus, &request) {
                hits += 1;
            } else {
                misses.push(format!(
                    "{category} {:?} expected {:?} missing from target request",
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
