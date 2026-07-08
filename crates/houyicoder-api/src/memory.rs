//! Memory ports: the engine-facing MemoryProvider trait plus the pluggable
//! MemoryStore backend seam. Signatures reference context payload types
//! (MemoryEntry, MemoryError). The concrete providers (markdown directory,
//! in-process keyword, Python sidecar) live in the memory crate; the engine
//! depends on these traits so it does not depend on any impl crate.
//!
//! The shape is deliberately small. recall is deterministic (keyword overlap
//! plus recency) so the hot path pays no per-turn model cost for ranking —
//! no model side-query per turn. write lands a single source of truth
//! (topic record lands or it does not, derived index pointer reconciled
//! under the same lock) rather than fanning a write out to three
//! independent paths where a crash between them leaves the store
//! half-written.
//!
//! De-dup is caller-driven, not provider-internal: the caller passes the set
//! of memory keys already in the served view (scanned from the projected
//! transcript) so recall skips entries the model already sees this turn.
//! Compaction folds old memory-recall events out of the projection (they
//! take the Summarized disposition), so the scanned surfaced set naturally
//! empties at the compaction boundary — the reset happens by projection,
//! with no provider-side mutable state to clear. Recall re-scans the
//! post-compact transcript for already-injected attachments rather than
//! tracking them in provider state — the reset is a natural consequence
//! of projection, not a separate cleanup step.
//!
//! MemoryStore is a thin backend seam: markdown is the default impl, a future
//! SqliteStore can impl the same seam with zero change to the recall engine
//! (the engine holds a provider trait object; the markdown provider wraps a
//! store trait object). No Sqlite/PG impl is built — markdown-only is
//! intentional; the seam is left open for million-scale later.

use houyicoder_context::{MemoryEntry, MemoryError, MemoryRecallStats, MemoryScope, MemorySummary};
use std::collections::HashSet;

/// Engine-facing recall plus write seam. The engine holds this trait and
/// never sees the backend. recall is budget-bounded deterministic ranking
/// that skips any key in surfaced (a key already in the served view this
/// turn); add lands a single source of truth; update rewrites; rebuild_index
/// regenerates the derived index from the topic files (self-healing).
pub trait MemoryProvider: Send + Sync {
    /// Recall entries fitting the budget, most relevant first, skipping any
    /// key in surfaced. The caller builds surfaced by scanning the projected
    /// transcript for already-injected memory-recall events, so the provider
    /// holds no surfaced state across calls.
    fn recall(&self, query: &str, budget: usize, surfaced: &HashSet<String>) -> Vec<MemoryEntry>;
    /// Atomically write a new memory entry (single source of truth; the
    /// derived index pointer is reconciled under the same lock; a failed
    /// pointer triggers best-effort rollback of the topic file).
    fn add(&self, entry: MemoryEntry) -> Result<(), MemoryError>;
    /// Atomically write a new memory entry into a specific storage scope.
    /// The default delegates to add (which writes to the auto scope for the
    /// markdown backend) so providers that do not distinguish scopes stay
    /// unchanged. Backends that host multiple scopes override this so a caller
    /// that wants to refresh a project-scope entry writes the topic into the
    /// project root, not a competing auto-scope copy that would shadow the
    /// explicit version by newest-mtime. This closes the dedup-divergence
    /// where a refresh of an explicit-scope memory would otherwise land in
    /// auto and the newer mtime would hide the explicit one.
    fn add_in_scope(&self, entry: MemoryEntry, scope: MemoryScope) -> Result<(), MemoryError> {
        let _ = scope;
        self.add(entry)
    }
    /// Promote a memory from the auto scope into the project scope (the
    /// always-on carrier). The dream calls this when a rule has crossed the
    /// promotion threshold (high recall frequency, or repeated gate
    /// violations). The operation merges the topic's rule sentence into the
    /// project memory file (agent.md), moves the topic file from the auto
    /// root into the project root so recall still finds it, and regenerates
    /// the derived indexes. Default NotFound for providers without a
    /// multi-scope filesystem backend.
    fn promote_memory(&self, _key: &str) -> Result<(), MemoryError> {
        Err(MemoryError::NotFound)
    }
    /// Demote a memory from the project scope back into the auto scope. The
    /// dream calls this when an always-on rule has decayed (long unstirred
    /// and no gate violations): removing it from the always-on carrier frees
    /// prefix budget while keeping the topic recallable. The operation
    /// removes the rule sentence from the project memory file, moves the
    /// topic file from the project root into the auto root, and regenerates
    /// the derived indexes. Default NotFound for providers without a
    ///   multi-scope filesystem backend.
    fn demote_memory(&self, _key: &str) -> Result<(), MemoryError> {
        Err(MemoryError::NotFound)
    }
    /// Rewrite an existing entry (rewrite topic file plus index pointer).
    /// Default delegates to add — backends that distinguish insert vs update
    /// override.
    fn update(&self, entry: MemoryEntry) -> Result<(), MemoryError> {
        self.add(entry)
    }
    /// Regenerate the derived index from the topic files (full rebuild,
    /// self-healing invariant). Default Ok for backends without a derived
    /// index.
    fn rebuild_index(&self) -> Result<(), MemoryError> {
        Ok(())
    }
    /// Rebuild the derived index only when it is stale (a topic file is newer
    /// than the index, or the index is missing). The self-healing check run
    /// on session start and on file-changed events so a crash or external
    /// edit between runs cannot leave a drifted index. No-op default for
    /// backends without a derived index.
    fn rebuild_if_stale(&self) -> Result<(), MemoryError> {
        Ok(())
    }
    /// List every stored memory as a frontmatter-only summary (key,
    /// description, source, mtime) — no body content, so a /memory browse
    /// stays cheap regardless of store size. Default empty for providers
    /// without a listing path (stubs); markdown overrides to scan the topic
    /// files. The full body is fetched on demand via show_memory.
    fn list_memories(&self) -> Vec<MemorySummary> {
        Vec::new()
    }
    /// Count topic files modified after the given timestamp (seconds since
    /// epoch). The consolidation dream uses this as the "has new material
    /// landed" gate — fire only when new memories arrived since the last
    /// dream, so a quiet store does not pay a forked LLM run to re-organize
    /// stable content. Default 0 for providers without a listing path (the
    /// delta gate then no-ops, falling back to time-only — stubs never fire
    /// anyway since memory_root is empty).
    fn count_new_since(&self, _since: u64) -> usize {
        0
    }
    /// Fetch the full body of one memory by key. Default None for providers
    /// without a read path (stubs); markdown overrides to read the topic file.
    fn show_memory(&self, _key: &str) -> Option<MemoryEntry> {
        None
    }
    /// The auto-scope write root as a string path. The consolidation dream uses
    /// this to locate the memory directory it consolidates and to place the
    /// consolidation lock. Empty string for providers without a filesystem
    /// root (the in-memory stub, test doubles) — the dream no-ops when the
    /// root is empty, so a non-filesystem backend never fires a dream.
    fn memory_root(&self) -> String {
        String::new()
    }
    /// Delete one memory by key. The consolidation dream calls this to prune
    /// stale or contradicted entries. Default returns NotFound for providers
    /// without a delete path (stubs); markdown overrides to remove the topic
    /// file and regenerate the derived index. The index rebuild happens once
    /// per delete (under the provider lock), so a batch prune pays one rebuild
    /// per call rather than one rebuild per dream. This form deletes from the
    /// auto scope (the dream's consolidation target).
    fn delete_memory(&self, _key: &str) -> Result<(), MemoryError> {
        Err(MemoryError::NotFound)
    }
    /// Delete one memory by key from a specific scope. The /memory pane calls
    /// this so pressing forget on a user/project row deletes the file in that
    /// scope's root, not just the auto-scope copy (which would leave the
    /// explicit original in place and the list would still show it). Default
    /// delegates to delete_memory: single-root providers treat every scope as
    /// the one root. Multi-scope filesystem backends override to route by
    /// root_for_scope, returning NotFound for a key absent in the chosen root
    /// (no silent auto fallback that would mask the missing explicit copy).
    fn delete_memory_in_scope(&self, key: &str, _scope: MemoryScope) -> Result<(), MemoryError> {
        self.delete_memory(key)
    }
    /// Read the advisory recall-frequency counters for every key. The
    /// consolidation dream reads this to nominate stale (low recall hits +
    /// old last access) entries for pruning. Default empty for providers
    /// without a sidecar (stubs); markdown overrides to read the sidecar.
    /// Advisory: a lost sidecar yields empty stats (cold restart), never an
    /// error — the counters re-accumulate.
    fn read_recall_stats(&self) -> Vec<MemoryRecallStats> {
        Vec::new()
    }
    /// Increment recall_hits and update last_access_ts for the given keys
    /// (the keys a recall just surfaced). Best-effort: a write failure logs
    /// and the counters re-accumulate next time, never failing the recall
    /// path. Default no-op for providers without a sidecar.
    fn record_recall_hits(&self, _keys: &[String]) {}
    /// Increment gate_violations for one key (signal B: a PreToolUse gate
    /// denied a call because the agent violated the rule that key names).
    /// The dream reads the cumulative count to nominate rules for
    /// promotion into the always-on carrier — closing the loop where a
    /// rule the agent keeps violating (but recall misses) gets promoted
    /// so it is always-on rather than recall-on-demand. Best-effort: a
    /// write failure logs and the counter re-accumulates, never failing
    /// the gate path. Default no-op for providers without a sidecar.
    fn record_gate_violation(&self, _key: &str) {}
}

/// Pluggable backend seam. markdown is the default impl; a future SqliteStore
/// impls the same seam with zero change to the recall engine. scan returns
/// entries (a lighter header-only type is a future refinement, deferred);
/// read returns the full entry body for a key; write lands the topic file
/// plus derived index pointer atomically; rebuild regenerates the index.
/// De-dup is caller-driven (surfaced on the provider seam), so the store
/// carries no de-dup state.
pub trait MemoryStore: Send + Sync {
    /// Phase1 scan: entry summaries (frontmatter plus light metadata, no
    /// full body read for ranking).
    fn scan(&self) -> Vec<MemoryEntry>;
    /// Phase3 read: the full entry body for a key.
    fn read(&self, key: &str) -> Result<MemoryEntry, MemoryError>;
    /// Atomically write a memory entry (topic file plus derived index
    /// pointer).
    fn write(&self, entry: MemoryEntry) -> Result<(), MemoryError>;
    /// Regenerate the derived index from the topic files (full rebuild).
    fn rebuild(&self) -> Result<(), MemoryError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both traits must support runtime dispatch.
    #[test]
    fn test_traits_are_object_safe() {
        let _provider: Box<dyn MemoryProvider> = Box::new(Stub);
        let _store: Box<dyn MemoryStore> = Box::new(Stub);
    }

    /// The default delete_memory_in_scope delegates to delete_memory so a
    /// single-root provider (the Stub) treats every scope as the one root.
    #[test]
    fn test_default_delete_in_scope() {
        let stub = Stub;
        assert!(
            matches!(
                stub.delete_memory_in_scope("any", MemoryScope::Project),
                Err(MemoryError::NotFound)
            ),
            "default delegates to delete_memory (NotFound for the Stub)"
        );
    }

    struct Stub;
    impl MemoryProvider for Stub {
        fn recall(
            &self,
            _query: &str,
            _budget: usize,
            _surfaced: &HashSet<String>,
        ) -> Vec<MemoryEntry> {
            Vec::new()
        }
        fn add(&self, _entry: MemoryEntry) -> Result<(), MemoryError> {
            Ok(())
        }
    }
    impl MemoryStore for Stub {
        fn scan(&self) -> Vec<MemoryEntry> {
            Vec::new()
        }
        fn read(&self, _key: &str) -> Result<MemoryEntry, MemoryError> {
            Err(MemoryError::NotFound)
        }
        fn write(&self, _entry: MemoryEntry) -> Result<(), MemoryError> {
            Ok(())
        }
        fn rebuild(&self) -> Result<(), MemoryError> {
            Ok(())
        }
    }

    #[test]
    fn test_store_stub_round_trip() {
        let store = Stub;
        assert!(store.scan().is_empty(), "scan returns empty by default");
        assert!(
            matches!(store.read("any"), Err(MemoryError::NotFound)),
            "read returns NotFound for the stub"
        );
        let entry = MemoryEntry::new("k", "content", houyicoder_context::MemorySource::User);
        assert!(store.write(entry).is_ok(), "write accepts the entry");
        assert!(store.rebuild().is_ok(), "rebuild succeeds");
    }
}
