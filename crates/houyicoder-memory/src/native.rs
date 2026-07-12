//! Native keyword-plus-recency recall provider.
//!
//! The no-sidecar path: a minimal store holding entries in a Vec behind a
//! Mutex. recall scores each entry by query-keyword overlap, breaks ties
//! by insertion order (later means newer), and returns as many entries as
//! fit the token budget. No embedding index; meant as the default store
//! and a test double. add appends under the lock so a half-write
//! is impossible.

use std::collections::HashSet;
use std::sync::Mutex;

use crate::provider::{hit_count, tokenize};
use houyicoder_api::memory::MemoryProvider;
use houyicoder_context::{MemoryEntry, MemoryError};

/// Minimal in-process memory store ranking by keyword overlap plus
/// insertion recency.
pub struct KeywordRecallProvider {
    entries: Mutex<Vec<MemoryEntry>>,
}

impl KeywordRecallProvider {
    /// Construct an empty store.
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
        }
    }

    /// Construct a store seeded with entries.
    pub fn with_entries(entries: Vec<MemoryEntry>) -> Self {
        Self {
            entries: Mutex::new(entries),
        }
    }

    /// Append an entry.
    pub fn push(&self, entry: MemoryEntry) {
        self.entries
            .lock()
            .expect("entries mutex poisoned")
            .push(entry);
    }
}

impl Default for KeywordRecallProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryProvider for KeywordRecallProvider {
    fn recall(&self, query: &str, budget: usize, surfaced: &HashSet<String>) -> Vec<MemoryEntry> {
        let keywords = tokenize(query);
        if keywords.is_empty() {
            return Vec::new();
        }
        let entries = self.entries.lock().expect("entries mutex poisoned");
        // Score each entry by the number of distinct query keywords it
        // contains; drop entries with no overlap or already in surfaced
        // (the caller-built de-dup set, scanned from the projected view).
        // Track insertion index for a recency tie-break (higher = newer).
        let mut scored: Vec<(u32, usize, MemoryEntry)> = entries
            .iter()
            .enumerate()
            .filter(|(_, e)| !surfaced.contains(&e.key))
            .filter_map(|(idx, e)| {
                let hits = hit_count(&e.content, &keywords);
                if hits == 0 {
                    return None;
                }
                Some((hits, idx, e.clone()))
            })
            .collect();
        // Relevance first, then recency (newer first).
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
        apply_budget(scored, budget)
    }

    fn add(&self, entry: MemoryEntry) -> Result<(), MemoryError> {
        self.entries
            .lock()
            .expect("entries mutex poisoned")
            .push(entry);
        Ok(())
    }
}

/// Greedily pack entries into the token budget. Entries are taken in
/// ranked order; an entry that would overflow the remaining budget stops
/// the walk (no skipping — rank order matters).
fn apply_budget(scored: Vec<(u32, usize, MemoryEntry)>, budget: usize) -> Vec<MemoryEntry> {
    if budget == 0 {
        return Vec::new();
    }
    let mut used: usize = 0;
    let mut out = Vec::new();
    for (_, _, entry) in scored.into_iter() {
        if used + entry.tokens > budget {
            break;
        }
        used += entry.tokens;
        out.push(entry);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_context::MemorySource;

    fn entry(key: &str, content: &str, source: MemorySource) -> MemoryEntry {
        MemoryEntry::new(key, content, source)
    }

    #[test]
    fn test_recall_returns_relevant_entries() {
        let provider = KeywordRecallProvider::with_entries(vec![
            entry("a", "the quick brown fox jumps", MemorySource::Project),
            entry(
                "b",
                "a totally unrelated note about cats",
                MemorySource::User,
            ),
            entry(
                "c",
                "fox sightings near the hen house",
                MemorySource::Project,
            ),
        ]);
        let out = provider.recall("fox brown", 1000, &HashSet::new());
        assert!(!out.is_empty());
        // The entry with both keywords ranks above the one with one.
        assert_eq!(out[0].key, "a");
        assert!(out.iter().any(|e| e.key == "c"));
        // The unrelated entry is filtered out.
        assert!(!out.iter().any(|e| e.key == "b"));
    }

    #[test]
    fn test_recall_respects_token_budget() {
        let provider = KeywordRecallProvider::with_entries(vec![
            entry("a", "alpha fox", MemorySource::Project),
            entry("b", "bravo fox", MemorySource::Project),
            entry("c", "charlie fox", MemorySource::Project),
        ]);
        // "alpha fox" is 9 chars -> 3 tokens. Budget 3 admits one entry.
        let out = provider.recall("fox", 3, &HashSet::new());
        assert_eq!(out.len(), 1);
        // Recency wins the tie: the newest single-hit entry comes first.
        assert_eq!(out[0].key, "c");
    }

    #[test]
    fn test_recall_zero_budget_returns() {
        let provider =
            KeywordRecallProvider::with_entries(vec![entry("a", "fox", MemorySource::User)]);
        assert!(provider.recall("fox", 0, &HashSet::new()).is_empty());
    }

    #[test]
    fn test_recall_empty_store_returns() {
        let provider = KeywordRecallProvider::new();
        assert!(
            provider
                .recall("anything", 1000, &HashSet::new())
                .is_empty()
        );
    }

    #[test]
    fn test_recall_empty_query_returns() {
        let provider =
            KeywordRecallProvider::with_entries(vec![entry("a", "fox", MemorySource::User)]);
        assert!(provider.recall("", 1000, &HashSet::new()).is_empty());
        assert!(provider.recall("   ", 1000, &HashSet::new()).is_empty());
    }

    #[test]
    fn test_recall_is_case_insensitive() {
        let provider = KeywordRecallProvider::with_entries(vec![entry(
            "a",
            "The FOX barks",
            MemorySource::User,
        )]);
        let out = provider.recall("fox", 1000, &HashSet::new());
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn test_recall_ranks_by_hit() {
        let provider = KeywordRecallProvider::with_entries(vec![
            entry("a", "fox fox fox", MemorySource::Project),
            entry("b", "fox and the hound", MemorySource::Project),
        ]);
        let out = provider.recall("fox hound", 1000, &HashSet::new());
        assert_eq!(out[0].key, "b");
        assert_eq!(out[1].key, "a");
    }

    #[test]
    fn test_add_then_recall() {
        let provider = KeywordRecallProvider::new();
        provider
            .add(entry(
                "k",
                "documented fox behavior",
                MemorySource::Feedback,
            ))
            .unwrap();
        let out = provider.recall("fox", 1000, &HashSet::new());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].key, "k");
        assert_eq!(out[0].source, MemorySource::Feedback);
    }

    #[test]
    fn test_add_preserves_tokens() {
        let provider = KeywordRecallProvider::new();
        let e = entry("k", "alpha fox body", MemorySource::Project);
        let expected_tokens = e.tokens;
        provider.add(e).unwrap();
        let out = provider.recall("fox", 1000, &HashSet::new());
        assert_eq!(out[0].tokens, expected_tokens);
    }
}
