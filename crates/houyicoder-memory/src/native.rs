//! Native keyword-plus-recency rank provider.
//!
//! The no-sidecar path: a minimal store holding entries in a Vec behind a
//! Mutex. rank scores each entry by query-keyword overlap and breaks ties
//! by insertion order (later means newer); zero-score entries stay listed
//! as candidates for the semantic stage. show_memory reads the stored
//! entry back unchanged, so the host materializes a selected body from
//! the same store that ranked it. add appends under the lock.

use std::collections::HashSet;
use std::sync::Mutex;

use crate::provider::{has_word_content, hit_count, tokenize};
use houyicoder_api::memory::MemoryProvider;
use houyicoder_context::{MemoryEntry, MemoryError, MemoryRankHit, MemoryScope};

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
    fn rank_candidates(&self, query: &str, surfaced: &HashSet<String>) -> Vec<MemoryRankHit> {
        let keywords = tokenize(query);
        // Mirrors the markdown provider: no word content ranks nothing, but
        // a query the tokenizer cannot split still offers its rows to the
        // semantic stage at score zero.
        if keywords.is_empty() && !has_word_content(query) {
            return Vec::new();
        }
        let entries = self.entries.lock().expect("entries mutex poisoned");
        // Score each entry by the number of distinct query keywords it
        // contains; skip entries already in surfaced (the caller-built
        // de-dup set, scanned from the projected view). Track insertion
        // index for a recency tie-break (higher = newer). Zero-score rows
        // stay in the list as the semantic stage's candidate input.
        let mut scored: Vec<(u32, usize, MemoryRankHit)> = entries
            .iter()
            .enumerate()
            .filter(|(_, e)| !surfaced.contains(&e.key))
            .map(|(idx, e)| {
                let hits = hit_count(&e.content, &keywords);
                let hit = MemoryRankHit::new(
                    e.key.clone(),
                    e.description.clone(),
                    e.source,
                    MemoryScope::Auto,
                    e.mtime_secs,
                    hits,
                );
                (hits, idx, hit)
            })
            .collect();
        // Relevance first, then recency (newer first).
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
        scored.into_iter().map(|(_, _, hit)| hit).collect()
    }

    fn show_memory(&self, key: &str) -> Option<MemoryEntry> {
        self.entries
            .lock()
            .expect("entries mutex poisoned")
            .iter()
            .find(|e| e.key == key)
            .cloned()
    }

    fn add(&self, entry: MemoryEntry) -> Result<(), MemoryError> {
        self.entries
            .lock()
            .expect("entries mutex poisoned")
            .push(entry);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_context::MemorySource;

    fn entry(key: &str, content: &str, source: MemorySource) -> MemoryEntry {
        MemoryEntry::new(key, content, source)
    }

    #[test]
    fn test_rank_orders_by_relevance() {
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
        let out = provider.rank_candidates("fox brown", &HashSet::new());
        // The entry with both keywords ranks above the one with one.
        assert_eq!(out[0].key, "a");
        assert_eq!(out[0].score, 2);
        assert_eq!(out[1].key, "c");
        assert_eq!(out[1].score, 1);
        // The unrelated entry stays listed with a zero score, sorted last —
        // it is still a candidate the semantic stage may select.
        assert_eq!(out[2].key, "b");
        assert_eq!(out[2].score, 0);
    }

    /// Mirrors the markdown provider: an unsplittable query still offers
    /// its rows at score zero, while punctuation-only ranks nothing.
    #[test]
    fn test_rank_unsplittable_offers_rows() {
        let provider = KeywordRecallProvider::with_entries(vec![entry(
            "tea",
            "how the team orders tea",
            MemorySource::Project,
        )]);
        let out = provider.rank_candidates("\u{8336}", &HashSet::new());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].score, 0);
        assert!(provider.rank_candidates("!!!", &HashSet::new()).is_empty());
    }

    #[test]
    fn test_rank_recency_breaks_tie() {
        let provider = KeywordRecallProvider::with_entries(vec![
            entry("a", "alpha fox", MemorySource::Project),
            entry("b", "bravo fox", MemorySource::Project),
            entry("c", "charlie fox", MemorySource::Project),
        ]);
        let out = provider.rank_candidates("fox", &HashSet::new());
        // Recency wins the tie: the newest single-hit entry comes first.
        let keys: Vec<&str> = out.iter().map(|h| h.key.as_str()).collect();
        assert_eq!(keys, vec!["c", "b", "a"]);
    }

    #[test]
    fn test_rank_skips_surfaced() {
        let provider = KeywordRecallProvider::with_entries(vec![
            entry("a", "alpha fox", MemorySource::Project),
            entry("b", "bravo fox", MemorySource::Project),
        ]);
        let mut surfaced = HashSet::new();
        surfaced.insert("a".to_string());
        let out = provider.rank_candidates("fox", &surfaced);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].key, "b");
    }

    #[test]
    fn test_rank_empty_store_returns() {
        let provider = KeywordRecallProvider::new();
        assert!(
            provider
                .rank_candidates("anything", &HashSet::new())
                .is_empty()
        );
    }

    #[test]
    fn test_rank_empty_query_returns() {
        let provider =
            KeywordRecallProvider::with_entries(vec![entry("a", "fox", MemorySource::User)]);
        assert!(provider.rank_candidates("", &HashSet::new()).is_empty());
        assert!(provider.rank_candidates("   ", &HashSet::new()).is_empty());
    }

    #[test]
    fn test_rank_is_case_insensitive() {
        let provider = KeywordRecallProvider::with_entries(vec![entry(
            "a",
            "The FOX barks",
            MemorySource::User,
        )]);
        let out = provider.rank_candidates("fox", &HashSet::new());
        assert_eq!(out[0].score, 1);
    }

    #[test]
    fn test_rank_prefers_more_hits() {
        let provider = KeywordRecallProvider::with_entries(vec![
            entry("a", "fox fox fox", MemorySource::Project),
            entry("b", "fox and the hound", MemorySource::Project),
        ]);
        let out = provider.rank_candidates("fox hound", &HashSet::new());
        assert_eq!(out[0].key, "b");
        assert_eq!(out[1].key, "a");
    }

    #[test]
    fn test_add_then_rank() {
        let provider = KeywordRecallProvider::new();
        provider
            .add(entry(
                "k",
                "documented fox behavior",
                MemorySource::Feedback,
            ))
            .unwrap();
        let out = provider.rank_candidates("fox", &HashSet::new());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].key, "k");
        assert_eq!(out[0].source, MemorySource::Feedback);
    }

    #[test]
    fn test_show_reads_back_entry() {
        let provider = KeywordRecallProvider::new();
        let e = entry("k", "alpha fox body", MemorySource::Project);
        let expected_tokens = e.tokens;
        provider.add(e).unwrap();
        let out = provider.show_memory("k").expect("stored entry reads back");
        assert_eq!(out.tokens, expected_tokens);
        assert_eq!(out.content, "alpha fox body");
        assert!(provider.show_memory("absent").is_none());
    }
}
