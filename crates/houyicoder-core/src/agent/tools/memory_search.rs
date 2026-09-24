//! The structured memory-search tool the main agent calls to find a stored
//! memory from a description of what it is about. The query goes through the
//! same rank the turn's recall uses, minus the surfaced filter and the body
//! budget; the provider owns the scan and the rank.
//!
//! Read-only by construction, so the approval gate stays off.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use houyicoder_api::memory::MemoryProvider;
use houyicoder_async::PFut;
use houyicoder_context::{MemorySummary, memory_age_days, memory_age_label};
use serde_json::{Map, Value, json};

use super::{Tool, ToolCtx, ToolError};

/// The body budget handed to the provider's rank. The rank truncates to its
/// own result cap before any body is packed, and the tool drops the bodies, so
/// the budget only has to not truncate a match.
const SEARCH_BODY_BUDGET: usize = usize::MAX;

/// A structured memory search. The agent calls it when the memory index lists
/// a candidate whose one-line description is not enough to judge.
pub struct SearchMemoryTool {
    provider: Arc<dyn MemoryProvider>,
}

impl SearchMemoryTool {
    /// Construct with a shared provider handle. The provider is shared with
    /// the runner memory, so a search reads the same store recall reads.
    pub fn new(provider: Arc<dyn MemoryProvider>) -> Self {
        Self { provider }
    }
}

impl Tool for SearchMemoryTool {
    fn name(&self) -> &str {
        "search_memory"
    }
    fn description(&self) -> &str {
        "Search stored memories for ones about a topic. Returns the matching \
         keys with their description, source, scope, and age. Use it when the \
         memory index shows a candidate you cannot judge from its description, \
         then read one body in full with show_memory."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "What the memory is about, described in your own words."
                }
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }
    fn execute(&self, _ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let provider = Arc::clone(&self.provider);
        Box::pin(async move {
            let query = input
                .get("query")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|q| !q.is_empty())
                .ok_or_else(|| {
                    ToolError::InvalidInput(
                        "search_memory: 'query' must be a non-empty string".to_string(),
                    )
                })?;
            // A search looks for a memory the agent may already hold in
            // context, so the surfaced filter the turn's recall applies is
            // left off here.
            let surfaced = HashSet::new();
            let ranked = provider.recall(query, SEARCH_BODY_BUDGET, &surfaced);
            if ranked.is_empty() {
                return Ok(json!({ "matches": [] }));
            }
            // The rank yields bodies; the reader needs the metadata that
            // chooses one. The summary index supplies the scope, which a body
            // does not carry.
            let index: HashMap<String, MemorySummary> = provider
                .list_memories()
                .into_iter()
                .map(|s| (s.key.clone(), s))
                .collect();
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let matches: Vec<Value> = ranked
                .iter()
                .map(|entry| match index.get(&entry.key) {
                    Some(s) => match_object(
                        &s.key,
                        &s.description,
                        s.source.as_label(),
                        Some(s.scope.as_label()),
                        s.mtime_secs,
                        now,
                    ),
                    None => match_object(
                        &entry.key,
                        &entry.description,
                        entry.source.as_label(),
                        None,
                        entry.mtime_secs,
                        now,
                    ),
                })
                .collect();
            Ok(json!({ "matches": matches }))
        })
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn is_destructive(&self) -> bool {
        false
    }
    /// Auto-approve: the tool is read-only, so there is no hard-to-reverse
    /// outward effect to gate.
    fn requires_approval(&self) -> bool {
        false
    }
}

/// One match row. The scope is omitted when the summary index holds no row for
/// the key, since an unnamed root is not a root and a placeholder would claim
/// a scope the provider never reported. The age is a human-readable label
/// because a raw timestamp makes the reader do arithmetic it does badly.
fn match_object(
    key: &str,
    description: &str,
    source: &str,
    scope: Option<&str>,
    mtime_secs: u64,
    now_secs: u64,
) -> Value {
    let mut row = Map::new();
    row.insert("key".to_string(), json!(key));
    row.insert("description".to_string(), json!(description));
    row.insert("source".to_string(), json!(source));
    if let Some(scope) = scope {
        row.insert("scope".to_string(), json!(scope));
    }
    row.insert(
        "age".to_string(),
        json!(memory_age_label(memory_age_days(mtime_secs, now_secs))),
    );
    Value::Object(row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_context::{MemoryEntry, MemoryError, MemorySource};
    use houyicoder_memory::MarkdownMemoryProvider;
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::process;
    use std::sync::Mutex;

    /// A provider that records the surfaced set it was called with, so a test
    /// can prove the search does not inherit the turn's surfaced filter.
    struct RecordingMemory {
        seen: Mutex<Vec<String>>,
    }

    impl MemoryProvider for RecordingMemory {
        fn recall(&self, _q: &str, _b: usize, surfaced: &HashSet<String>) -> Vec<MemoryEntry> {
            *self.seen.lock().expect("seen") = surfaced.iter().cloned().collect();
            Vec::new()
        }
        fn add(&self, _e: MemoryEntry) -> Result<(), MemoryError> {
            Ok(())
        }
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

    /// The scope comes from the summary index, and a key the index does not
    /// hold reports no scope rather than a placeholder root.
    #[tokio::test]
    async fn test_search_omits_unknown_scope() {
        struct RankedOnly;
        impl MemoryProvider for RankedOnly {
            fn recall(&self, _q: &str, _b: usize, _s: &HashSet<String>) -> Vec<MemoryEntry> {
                vec![entry("ghost", "ranked but unlisted", "body")]
            }
            fn add(&self, _e: MemoryEntry) -> Result<(), MemoryError> {
                Ok(())
            }
            fn list_memories(&self) -> Vec<MemorySummary> {
                Vec::new()
            }
        }
        let tool = SearchMemoryTool::new(Arc::new(RankedOnly) as Arc<dyn MemoryProvider>);
        let out = run(&tool, json!({ "query": "ghost" }))
            .await
            .expect("search");
        let matches = out
            .get("matches")
            .and_then(|m| m.as_array())
            .expect("matches");
        assert_eq!(matches.len(), 1);
        assert!(
            matches[0].get("scope").is_none(),
            "an unlisted key carries no scope: {}",
            matches[0]
        );
        assert_eq!(
            matches[0].get("description").and_then(|v| v.as_str()),
            Some("ranked but unlisted")
        );
    }
}
