//! Tests for this module.

use super::*;
use houyicoder_api::memory::RerankOutcome;
use houyicoder_context::{MemoryEntry, MemoryError, MemoryScope, MemorySource};
use houyicoder_memory::MarkdownMemoryProvider;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A provider that records the surfaced set it was called with, so a test
/// can prove the search does not inherit the turn's surfaced filter.
struct RecordingMemory {
    seen: Mutex<Vec<String>>,
}

impl MemoryProvider for RecordingMemory {
    fn rank_candidates(&self, _q: &str, surfaced: &HashSet<String>) -> Vec<MemoryRankHit> {
        *self.seen.lock().expect("seen") = surfaced.iter().cloned().collect();
        Vec::new()
    }
    fn add(&self, _e: MemoryEntry) -> Result<(), MemoryError> {
        Ok(())
    }
}

/// A reranker answering one canned outcome and counting its calls.
struct StubReranker {
    outcome: RerankOutcome,
    calls: AtomicUsize,
}

impl MemoryReranker for StubReranker {
    fn rerank(&self, _q: &str, _c: &[MemoryRankHit], _limit: usize) -> PFut<'_, RerankOutcome> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let outcome = self.outcome.clone();
        Box::pin(async move { outcome })
    }
}

/// A rank stub answering fixed rows, so a test controls the lexical
/// verdict precisely.
struct RankedRows(Vec<MemoryRankHit>);

impl MemoryProvider for RankedRows {
    fn rank_candidates(&self, _q: &str, _s: &HashSet<String>) -> Vec<MemoryRankHit> {
        self.0.clone()
    }
    fn add(&self, _e: MemoryEntry) -> Result<(), MemoryError> {
        Ok(())
    }
}

fn hit(key: &str, score: u32) -> MemoryRankHit {
    MemoryRankHit::new(
        key,
        format!("{key} description"),
        MemorySource::Project,
        MemoryScope::Project,
        0,
        score,
    )
}

fn root(name: &str) -> PathBuf {
    let dir = env::temp_dir().join(format!("memory-search-{}-{name}", process::id()));
    fs::remove_dir_all(&dir).ok();
    fs::create_dir_all(&dir).expect("create memory root");
    dir
}

/// A topic as both paths see it. The writer derives the frontmatter
/// description from the first line of the body and the reader hands that
/// description back, so the two cannot be set independently: the
/// description is the body's opening line.
fn entry(key: &str, description: &str, body: &str) -> MemoryEntry {
    MemoryEntry::new(key, format!("{description}\n{body}"), MemorySource::Project)
        .with_meta(description.to_string(), 0)
}

async fn run(tool: &SearchMemoryTool, input: Value) -> Result<Value, ToolError> {
    tool.execute(ToolCtx::new("test"), input).await
}

/// The query reaches the provider's rank and the top match is the memory
/// about it, with the fields a reader chooses from.
#[tokio::test]
async fn test_search_ranks_by_query() {
    let dir = root("rank");
    let provider = Arc::new(MarkdownMemoryProvider::new(dir.clone()));
    provider
        .add(entry(
            "deploy-gate",
            "the deploy gate is red until review",
            "body one",
        ))
        .expect("add deploy-gate");
    provider
        .add(entry("tea-order", "how the team orders tea", "body two"))
        .expect("add tea-order");
    let tool = SearchMemoryTool::new(Arc::clone(&provider) as Arc<dyn MemoryProvider>);
    let out = run(&tool, json!({ "query": "deploy gate review" }))
        .await
        .expect("search");
    let matches = out
        .get("matches")
        .and_then(|m| m.as_array())
        .expect("matches");
    assert_eq!(matches.len(), 1, "only the memory about the query matches");
    assert_eq!(
        matches[0].get("key").and_then(|v| v.as_str()),
        Some("deploy-gate")
    );
    assert_eq!(
        matches[0].get("description").and_then(|v| v.as_str()),
        Some("the deploy gate is red until review")
    );
    assert_eq!(
        matches[0].get("source").and_then(|v| v.as_str()),
        Some("project")
    );
    assert_eq!(
        matches[0].get("scope").and_then(|v| v.as_str()),
        Some("user")
    );
    assert_eq!(
        matches[0].get("age").and_then(|v| v.as_str()),
        Some("today")
    );
    assert!(
        !out.to_string().contains("body one"),
        "a search reports metadata, not bodies"
    );
    fs::remove_dir_all(&dir).ok();
}

/// A query with no lexical overlap yields an empty match list rather than
/// an error or an unrelated memory.
#[tokio::test]
async fn test_search_no_match_empty() {
    let dir = root("empty");
    let provider = Arc::new(MarkdownMemoryProvider::new(dir.clone()));
    provider
        .add(entry("deploy-gate", "the deploy gate is red", "body"))
        .expect("add");
    let tool = SearchMemoryTool::new(Arc::clone(&provider) as Arc<dyn MemoryProvider>);
    let out = run(&tool, json!({ "query": "kettle whistle" }))
        .await
        .expect("search");
    assert_eq!(out, json!({ "matches": [] }));
    fs::remove_dir_all(&dir).ok();
}

/// A blank query is a caller error, not a silent empty search.
#[tokio::test]
async fn test_search_rejects_blank_query() {
    let dir = root("blank");
    let provider = Arc::new(MarkdownMemoryProvider::new(dir.clone()));
    let tool = SearchMemoryTool::new(Arc::clone(&provider) as Arc<dyn MemoryProvider>);
    for input in [json!({}), json!({ "query": "" }), json!({ "query": "   " })] {
        let err = run(&tool, input.clone())
            .await
            .expect_err("blank query must fail");
        assert!(
            matches!(err, ToolError::InvalidInput(_)),
            "a blank query is a caller error: {err:?}"
        );
        assert!(
            err.to_string().contains("non-empty"),
            "the error names the contract: {err}"
        );
    }
    fs::remove_dir_all(&dir).ok();
}

/// The turn's recall skips memories already in context; an explicit search
/// must not, since the agent is asking for one it may already have seen.
#[tokio::test]
async fn test_search_ignores_surfaced() {
    let memory = Arc::new(RecordingMemory {
        seen: Mutex::new(Vec::new()),
    });
    let tool = SearchMemoryTool::new(Arc::clone(&memory) as Arc<dyn MemoryProvider>);
    run(&tool, json!({ "query": "anything" }))
        .await
        .expect("search");
    assert!(
        memory.seen.lock().expect("seen").is_empty(),
        "the search passes an empty surfaced set to the rank"
    );
}

/// Every row carries the scope of the root the rank found it in; no
/// second index lookup can leave a row without one.
#[tokio::test]
async fn test_search_rows_carry_scope() {
    let tool = SearchMemoryTool::new(
        Arc::new(RankedRows(vec![hit("listed", 3)])) as Arc<dyn MemoryProvider>
    );
    let out = run(&tool, json!({ "query": "listed" }))
        .await
        .expect("search");
    let matches = out
        .get("matches")
        .and_then(|m| m.as_array())
        .expect("matches");
    assert_eq!(matches.len(), 1);
    assert_eq!(
        matches[0].get("scope").and_then(|v| v.as_str()),
        Some("project"),
        "the scope is the root the rank found: {}",
        matches[0]
    );
    assert_eq!(
        matches[0].get("description").and_then(|v| v.as_str()),
        Some("listed description")
    );
}

/// A weak lexical signal goes through the semantic stage inline, and the
/// model's relevance order is the row order — even when it inverts the
/// lexical ranking.
#[tokio::test]
async fn test_search_reranks_weak_signal() {
    let provider: Arc<dyn MemoryProvider> =
        Arc::new(RankedRows(vec![hit("tea-order", 1), hit("deploy-gate", 1)]));
    let reranker: Arc<dyn MemoryReranker> = Arc::new(StubReranker {
        outcome: RerankOutcome::Selected(vec!["deploy-gate".into()]),
        calls: AtomicUsize::new(0),
    });
    let tool = SearchMemoryTool::new(provider).with_reranker(Some(reranker));
    let out = run(&tool, json!({ "query": "deploy question" }))
        .await
        .expect("search");
    let matches = out
        .get("matches")
        .and_then(|m| m.as_array())
        .expect("matches");
    assert_eq!(matches.len(), 1, "the semantic verdict picks the row");
    assert_eq!(
        matches[0].get("key").and_then(|v| v.as_str()),
        Some("deploy-gate")
    );
}

/// A confident semantic empty verdict reports no matches; the weak
/// lexical rows the model rejected must not be shown.
#[tokio::test]
async fn test_search_semantic_empty() {
    let provider: Arc<dyn MemoryProvider> = Arc::new(RankedRows(vec![hit("tea-order", 1)]));
    let reranker: Arc<dyn MemoryReranker> = Arc::new(StubReranker {
        outcome: RerankOutcome::Selected(Vec::new()),
        calls: AtomicUsize::new(0),
    });
    let tool = SearchMemoryTool::new(provider).with_reranker(Some(reranker));
    let out = run(&tool, json!({ "query": "kettle" }))
        .await
        .expect("search");
    assert_eq!(out, json!({ "matches": [] }));
}

/// A failed semantic selection degrades to the deterministic lexical
/// rows, so an explicit search still answers when the model cannot.
#[tokio::test]
async fn test_search_rerank_failure_lexical() {
    let provider: Arc<dyn MemoryProvider> =
        Arc::new(RankedRows(vec![hit("tea-order", 1), hit("deploy-gate", 0)]));
    let reranker: Arc<dyn MemoryReranker> = Arc::new(StubReranker {
        outcome: RerankOutcome::Unavailable("no route".into()),
        calls: AtomicUsize::new(0),
    });
    let tool = SearchMemoryTool::new(provider).with_reranker(Some(reranker));
    let out = run(&tool, json!({ "query": "tea" })).await.expect("search");
    let matches = out
        .get("matches")
        .and_then(|m| m.as_array())
        .expect("matches");
    assert_eq!(matches.len(), 1);
    assert_eq!(
        matches[0].get("key").and_then(|v| v.as_str()),
        Some("tea-order"),
        "the lexical fallback keeps the matching row"
    );
}

/// A confident lexical rank answers without spending a semantic call.
#[tokio::test]
async fn test_search_confident_no_rerank() {
    let provider: Arc<dyn MemoryProvider> = Arc::new(RankedRows(vec![hit("deploy-gate", 3)]));
    let reranker: Arc<StubReranker> = Arc::new(StubReranker {
        outcome: RerankOutcome::Selected(Vec::new()),
        calls: AtomicUsize::new(0),
    });
    let tool = SearchMemoryTool::new(provider)
        .with_reranker(Some(Arc::clone(&reranker) as Arc<dyn MemoryReranker>));
    let out = run(&tool, json!({ "query": "deploy gate" }))
        .await
        .expect("search");
    let matches = out
        .get("matches")
        .and_then(|m| m.as_array())
        .expect("matches");
    assert_eq!(matches.len(), 1);
    assert_eq!(
        reranker.calls.load(Ordering::SeqCst),
        0,
        "a confident rank must not spend a semantic call"
    );
}
