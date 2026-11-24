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
    fn rank_candidates(&self, query: &str, injected_keys: &HashSet<String>) -> Vec<MemoryRankHit> {
        let keywords = tokenize(query);
        if keywords.is_empty() && !has_word_content(query) {
            return Vec::new();
        }
        let entries = self.entries.lock().expect("entries mutex poisoned");
        let mut scored: Vec<(u32, usize, MemoryRankHit)> = entries
            .iter()
            .enumerate()
            .filter(|(_, e)| !injected_keys.contains(&e.key))
            .map(|(idx, e)| {
                let hits = hit_count(&e.content, &keywords);
                let hit = MemoryRankHit::new(
                    e.key.clone(),
                    e.description.clone(),
                    e.memory_type,
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
mod tests;
