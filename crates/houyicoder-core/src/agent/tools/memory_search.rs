//! Structured memory-search tool used to find stored memories by topic.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use houyicoder_api::memory::{MemoryProvider, MemoryReranker};
use houyicoder_async::PFut;
use houyicoder_context::{MemoryRankHit, memory_age_days, memory_age_label};
use serde_json::{Map, Value, json};

use super::{Tool, ToolCtx, ToolError};
use crate::agent::memory::selector::{RECALL_SELECT_CAP, classify, run_semantic_selection};

/// Structured memory search tool.
pub struct MemorySearchTool {
    provider: Arc<dyn MemoryProvider>,
    reranker: Option<Arc<dyn MemoryReranker>>,
}

impl MemorySearchTool {
    /// Construct with a shared provider handle.
    pub fn new(provider: Arc<dyn MemoryProvider>) -> Self {
        Self {
            provider,
            reranker: None,
        }
    }

    /// Install the semantic selection stage.
    pub fn with_reranker(mut self, reranker: Option<Arc<dyn MemoryReranker>>) -> Self {
        self.reranker = reranker;
        self
    }
}

impl Tool for MemorySearchTool {
    fn name(&self) -> &str {
        "memory_search"
    }
    fn description(&self) -> &str {
        "Search stored memories for ones about a topic. Returns the matching \
         keys with their description, type, scope, and age. Use it when the \
         memory index shows a candidate you cannot judge from its description, \
         then read one body in full with memory_read."
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
        let reranker = self.reranker.clone();
        Box::pin(async move {
            let query = input
                .get("query")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|q| !q.is_empty())
                .ok_or_else(|| {
                    ToolError::InvalidInput(
                        "memory_search: 'query' must be a non-empty string".to_string(),
                    )
                })?;
            let scored = provider.rank_candidates(query, &HashSet::new());
            if scored.is_empty() {
                return Ok(json!({ "matches": [] }));
            }
            // A weak lexical signal waits for the semantic verdict; its row
            // order is the model's relevance order. A failed selection keeps
            // the deterministic lexical answer.
            let mut semantic_rows: Option<Vec<&MemoryRankHit>> = None;
            if classify(&scored).needs_rerank()
                && let Some(reranker) = reranker.as_ref()
            {
                let (selected, fallback) =
                    run_semantic_selection(Arc::clone(reranker), query.to_string(), &scored).await;
                if fallback.is_none() {
                    semantic_rows = Some(
                        selected
                            .iter()
                            .filter_map(|k| scored.iter().find(|h| &h.key == k))
                            .collect(),
                    );
                }
            }
            let rows: Vec<&MemoryRankHit> = semantic_rows.unwrap_or_else(|| {
                scored
                    .iter()
                    .filter(|h| h.score > 0)
                    .take(RECALL_SELECT_CAP)
                    .collect()
            });
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let matches: Vec<Value> = rows.iter().map(|h| match_object(h, now)).collect();
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

/// One match row. The rank hit carries every column directly — key,
/// description, source, and the scope of the root it was found in — so no
/// second index lookup can disagree with the rank. The age is a
/// human-readable label because a raw timestamp makes the reader do
/// arithmetic it does badly.
fn match_object(hit: &MemoryRankHit, now_secs: u64) -> Value {
    let mut row = Map::new();
    row.insert("key".to_string(), json!(hit.key));
    row.insert("description".to_string(), json!(hit.description));
    row.insert("type".to_string(), json!(hit.memory_type.as_label()));
    row.insert("scope".to_string(), json!(hit.scope.as_label()));
    row.insert(
        "age".to_string(),
        json!(memory_age_label(memory_age_days(hit.mtime_secs, now_secs))),
    );
    Value::Object(row)
}

#[cfg(test)]
mod tests;
