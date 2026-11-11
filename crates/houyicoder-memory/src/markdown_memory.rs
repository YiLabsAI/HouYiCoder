//! Markdown directory plus derived-index memory provider.
//!
//! Topic files are the source of truth; MEMORY.md is a bounded derived index.
//! Recall ranks frontmatter by keyword overlap and reads selected bodies only.
//! Writes compare and replace a topic under a cross-process root lock, then
//! reconcile the index. A writer marks the index dirty before changing a topic
//! and clears the marker after publishing, so a failed update heals on the
//! next rebuild. Rendering never performs synchronous reads.

use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use std::cmp::Reverse;

use crate::provider::{has_word_content, hit_count, tokenize};
use houyicoder_api::memory::{MemoryProvider, MemoryWriteOutcome};
use houyicoder_context::{
    MemoryEntry, MemoryError, MemoryRankHit, MemoryRecallStats, MemoryScope, MemorySummary,
};

mod index_health;
mod io;
mod lock;
mod roots;
mod scope_flow;
use io::{
    Frontmatter, INDEX_BYTE_CAP, INDEX_FILE, ScannedTopic, StatRecord, cap_index_content,
    count_new_since_impl, first_line, list_memories_impl, parse_topic_file, sanitize_key,
    scopes_for_key_impl, serialize_topic_file, show_memory_in_scope_impl, validate_index_key,
    write_bytes_advisory, write_bytes_atomic,
};
use lock::RootLock;
use roots::canonical_identity;
use std::io::ErrorKind;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// Cap on candidate files scanned per rank; keeps the scan bounded.
const SCAN_FILE_CAP: usize = 200;

/// Cap on lines in the derived MEMORY index so it never grows past a
/// bounded number of entries even with short lines.
const MAX_ENTRYPOINT_LINES: usize = 200;

/// Advisory recall-frequency sidecar filename. Lives in the write root
/// alongside the topic files and the derived index, written atomically under
/// that root's lock. Advisory: a lost or corrupt sidecar is a cold restart
/// (counters re-accumulate), never an invariant violation.
const STATS_FILE: &str = ".stats.json";

/// Markdown directory memory provider with a derived index. Holds an
/// ordered list of roots (user, project, auto); recall scans every root
/// and merges, write lands in the last root (the auto/write scope).
pub struct MarkdownMemoryProvider {
    roots: Vec<PathBuf>,
    write_lock: Mutex<()>,
    max_lines: usize,
}

impl MarkdownMemoryProvider {
    /// Construct a single-root provider. Used by tests and any single-scope
    /// caller; the root is both the scan root and the write target.
    pub fn new(root: PathBuf) -> Self {
        Self::with_max_lines(vec![root], MAX_ENTRYPOINT_LINES)
    }

    /// Construct a multi-root provider. Roots are scanned in order (user,
    /// project, auto) and merged for recall; writes land in the last root.
    /// Empty roots are dropped, and two spellings of one directory dedupe to
    /// the first, so a degenerate config cannot scan one root twice or claim
    /// its lock twice. A missing root is treated as no candidates (created on
    /// demand at write time only for the write root).
    pub fn new_multi(roots: Vec<PathBuf>) -> Self {
        Self::with_max_lines(roots, MAX_ENTRYPOINT_LINES)
    }

    /// Construct with a custom index line cap. Tests use a small value so the
    /// line-cap path is exercised without hundreds of file writes. Roots are
    /// deduped by resolved identity, first wins, so the higher-priority scope
    /// survives; one root claimed twice under two names would otherwise wait
    /// on its own lock. Empty roots are dropped.
    pub fn with_max_lines(roots: Vec<PathBuf>, max_lines: usize) -> Self {
        let mut seen: HashSet<PathBuf> = HashSet::new();
        let mut roots: Vec<PathBuf> = roots
            .into_iter()
            .filter(|root| !root.as_os_str().is_empty())
            .filter(|root| seen.insert(canonical_identity(root)))
            .collect();
        if roots.is_empty() {
            roots.push(PathBuf::new());
        }
        Self {
            roots,
            write_lock: Mutex::new(()),
            max_lines,
        }
    }

    /// The write target root (the last root, i.e. the auto scope).
    fn write_root(&self) -> &Path {
        self.roots.last().expect("at least one root is required")
    }
    fn topic_path(&self, key: &str) -> PathBuf {
        self.write_root().join(format!("{key}.md"))
    }

    fn index_path(&self) -> PathBuf {
        self.write_root().join(INDEX_FILE)
    }

    fn index_path_for(root: &Path) -> PathBuf {
        root.join(INDEX_FILE)
    }

    /// The advisory stats sidecar path in the write root.
    fn stats_path(&self) -> PathBuf {
        self.write_root().join(STATS_FILE)
    }

    /// Load the advisory stats sidecar. A missing or corrupt sidecar yields
    /// an empty map (cold restart) — never an error, since the sidecar is
    /// advisory and re-accumulates.
    fn load_stats(&self) -> HashMap<String, StatRecord> {
        let Ok(text) = fs::read_to_string(self.stats_path()) else {
            return HashMap::new();
        };
        serde_json::from_str(&text).unwrap_or_default()
    }

    /// Persist the stats map best-effort through the atomic write, called
    /// with the write root's lock held. Counters are advisory, so a failed or
    /// lost update is a cold restart for them rather than an invariant
    /// violation. The write skips the durability flush for the same reason,
    /// and a failure reaches diagnostics.
    fn save_stats(&self, stats: &HashMap<String, StatRecord>) {
        let Ok(json) = serde_json::to_string(stats) else {
            return;
        };
        if write_bytes_advisory(&self.stats_path(), json.as_bytes()).is_err() {
            tracing::warn!("memory recall stats could not be persisted (advisory)");
        }
    }

    /// True when a root's derived index is stale: the index is missing, or any
    /// topic file is newer than the index (an external edit or a crashed write
    /// landed a topic after the index pointer). A missing root is not stale
    /// (nothing to rebuild); only an existing root with a drifted index is.
    fn root_is_stale(root: &Path) -> bool {
        let idx_meta = match fs::metadata(Self::index_path_for(root)) {
            Ok(m) => m,
            Err(_) => {
                // No index yet: stale only if topic files exist with no index.
                return fs::read_dir(root)
                    .map(|e| {
                        e.flatten().any(|d| {
                            d.path().extension().and_then(|x| x.to_str()) == Some("md")
                                && d.path().file_name().is_none_or(|n| n != INDEX_FILE)
                        })
                    })
                    .unwrap_or(false);
            }
        };
        let idx_mtime = match idx_meta.modified() {
            Ok(t) => t,
            Err(_) => return true,
        };
        let entries = match fs::read_dir(root) {
            Ok(e) => e,
            Err(_) => return false,
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("md") {
                continue;
            }
            if path.file_name().is_some_and(|n| n == INDEX_FILE) {
                continue;
            }
            let Ok(m) = e.metadata() else { continue };
            let Ok(t) = m.modified() else { continue };
            if t > idx_mtime {
                return true;
            }
        }
        false
    }

    /// Scan one root's topic files newest-first (mtime descending),
    /// frontmatter only, returning candidates with precomputed ranking
    /// fields. A missing root yields no candidates.
    fn scan_root(root: &Path) -> Vec<ScannedTopic> {
        let entries = match fs::read_dir(root) {
            Ok(e) => e,
            Err(_) => return Vec::new(),
        };
        let mut candidates: Vec<ScannedTopic> = entries
            .flatten()
            .filter_map(|e| {
                let path = e.path();
                if path.extension().and_then(|x| x.to_str()) != Some("md") {
                    return None;
                }
                // Skip the derived index file itself.
                if path.file_name().is_some_and(|n| n == INDEX_FILE) {
                    return None;
                }
                let stem = path.file_stem()?.to_str()?.to_string();
                let meta = e.metadata().ok()?;
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                Some(ScannedTopic {
                    key: stem,
                    mtime,
                    path,
                    root_index: 0,
                })
            })
            .collect();
        // The key is the tiebreaker when mtimes collide (a batch write gives
        // many topics the same second), so the scan order is a function of
        // the store contents alone. read_dir order is unspecified and must
        // not leak into the ranking: two runtimes rebuilding the index from
        // one store would otherwise emit different prompts.
        candidates.sort_by_key(|c| Reverse((c.mtime, c.key.clone())));
        candidates
    }

    /// Scan every root and merge. When the same key appears in more than one
    /// scope, an explicit scope (user or project) wins over the automatic
    /// write scope regardless of modification time: the user's own statement
    /// is not something a later automatic write may quietly replace. Within
    /// one rank the newest mtime wins, so recall does not return duplicates.
    fn scan_candidates(&self) -> Vec<ScannedTopic> {
        let mut by_key: HashMap<String, ScannedTopic> = HashMap::new();
        for (root_index, root) in self.roots.iter().enumerate() {
            let rank = u8::from(!self.is_explicit_scope(root_index));
            for mut t in Self::scan_root(root) {
                t.root_index = root_index;
                let keep = match by_key.get(&t.key) {
                    None => true,
                    Some(e) => {
                        let existing_rank = u8::from(!self.is_explicit_scope(e.root_index));
                        (rank, Reverse(t.mtime)) < (existing_rank, Reverse(e.mtime))
                    }
                };
                if keep {
                    by_key.insert(t.key.clone(), t);
                }
            }
        }
        let mut merged: Vec<ScannedTopic> = by_key.into_values().collect();
        // Explicit scopes first, then newest, then key, so the merged order
        // depends only on the store contents and never on HashMap or read_dir
        // iteration order. The index cap truncates the tail, so the ordering
        // is what keeps a promoted memory from being pushed out by the
        // automatic writer's churn.
        merged.sort_by_key(|c| {
            (
                !self.is_explicit_scope(c.root_index),
                Reverse(c.mtime),
                Reverse(c.key.clone()),
            )
        });
        merged
    }

    /// Rebuild the derived MEMORY index for every root from that root's own
    /// topic files. The index is derived: it can always be regenerated
    /// from the single-source topic records. Inherent helper called by the
    /// trait rebuild_index override (avoids same-name dispatch ambiguity).
    fn rebuild_index_impl(&self) -> Result<(), MemoryError> {
        let _guard = self.write_lock.lock().expect("write lock poisoned");
        let _root_locks = self.lock_roots()?;
        self.rebuild_index_impl_locked()
    }

    /// rebuild_index_impl assumed the caller already holds the in-process
    /// write lock and every root lock. Used by promote, demote, and delete so
    /// the rebuild runs under the same locks as the topic change — closing
    /// the window where a concurrent writer could land a competing copy
    /// between the change and the rebuild.
    fn rebuild_index_impl_locked(&self) -> Result<(), MemoryError> {
        for root in &self.roots {
            fs::create_dir_all(root).map_err(|_| MemoryError::Io)?;
            // The marker goes down before this root's index is replaced, and
            // comes off only after the new index landed. A failure part way
            // through the roots therefore leaves the root being rebuilt
            // marked for the next session; a root not reached is untouched.
            Self::mark_index_dirty(root)?;
            let topics = Self::scan_root(root);
            let mut out = String::from("# Memory index\n\n");
            for t in topics.iter().take(SCAN_FILE_CAP) {
                // A key no row can address stays out of the index rather
                // than publishing a row that reads back as another key.
                if !Self::index_key_representable(&t.key) {
                    continue;
                }
                let line = match self.read_frontmatter(&t.path) {
                    Ok(fm) => format!(
                        "- {} [{}]: {}\n",
                        t.key,
                        fm.source.as_label(),
                        fm.description
                    ),
                    Err(_) => continue,
                };
                out.push_str(&line);
                if out.len() > INDEX_BYTE_CAP || out.lines().count() > self.max_lines {
                    break;
                }
            }
            out = cap_index_content(&out, self.max_lines);
            write_bytes_atomic(&Self::index_path_for(root), out.as_bytes())?;
            Self::clear_index_dirty(root);
        }
        Ok(())
    }

    fn read_frontmatter(&self, path: &Path) -> Result<Frontmatter, MemoryError> {
        let text = fs::read_to_string(path).map_err(|e| match e.kind() {
            ErrorKind::NotFound => MemoryError::NotFound,
            _ => MemoryError::Io,
        })?;
        let (fm, _body) = parse_topic_file(&text)?;
        Ok(fm)
    }

    /// Read the full topic record (frontmatter plus body) and map it
    /// into a MemoryEntry.
    fn read_topic(&self, path: &Path) -> Result<MemoryEntry, MemoryError> {
        let text = fs::read_to_string(path).map_err(|e| match e.kind() {
            ErrorKind::NotFound => MemoryError::NotFound,
            _ => MemoryError::Io,
        })?;
        let (fm, body) = parse_topic_file(&text)?;
        let key = path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| MemoryError::Corrupt("unmappable file stem".into()))?
            .to_string();
        let mtime_secs = fs::metadata(path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Ok(MemoryEntry::new(key, body, fm.source).with_meta(fm.description.clone(), mtime_secs))
    }

    /// Append (or write) an index pointer for the entry in the given root.
    /// Inherent helper so add + add_in_scope share one append path.
    fn append_index_pointer_in(&self, root: &Path, entry: &MemoryEntry) -> Result<(), String> {
        let index_path = Self::index_path_for(root);
        let mut existing = fs::read_to_string(&index_path).unwrap_or_default();
        let prefix = format!("- {} [", entry.key);
        existing = existing
            .lines()
            .filter(|line| !line.starts_with(&prefix))
            .map(|line| format!("{line}\n"))
            .collect();
        existing.push_str(&format!(
            "- {} [{}]: {}\n",
            entry.key,
            entry.source.as_label(),
            first_line(&entry.content)
        ));
        existing = cap_index_content(&existing, self.max_lines);
        write_bytes_atomic(&index_path, existing.as_bytes()).map_err(|e| e.to_string())
    }

    /// Whether a root holds an explicit scope (user or project) rather than
    /// the automatic write scope. An explicit scope is the user's own
    /// statement, so it outranks anything the automatic writer produced.
    fn is_explicit_scope(&self, root_index: usize) -> bool {
        root_index + 1 < self.roots.len()
    }
}

impl MemoryProvider for MarkdownMemoryProvider {
    /// Regenerate the derived index from all topic files (a full,
    /// self-healing rebuild).
    fn rebuild_index(&self) -> Result<(), MemoryError> {
        self.rebuild_index_impl()
    }

    /// Rebuild only when a root's index no longer describes its topic set:
    /// a writer left its marker, the index is missing or older than a topic,
    /// or a row names a deleted topic. Run on session start and before the
    /// dream. The check and the rebuild run under the locks a writer takes,
    /// and the check is repeated inside them, so a write that lands while a
    /// session starts cannot be judged against a state already gone.
    fn rebuild_if_stale(&self) -> Result<(), MemoryError> {
        let _guard = self.write_lock.lock().expect("write lock poisoned");
        let _root_locks = self.lock_roots()?;
        if self.roots.iter().any(|root| self.index_needs_rebuild(root)) {
            self.rebuild_index_impl_locked()?;
        }
        Ok(())
    }

    /// List every topic as a frontmatter-only summary — key, description,
    /// source, mtime — without reading any body content, so a /memory browse
    /// stays cheap regardless of store size. Roots are merged (newest mtime
    /// wins per key, matching recall) so the listing never shows a stale
    /// duplicate of a key that a newer scope overrides.
    fn list_memories(&self) -> Vec<MemorySummary> {
        list_memories_impl(self)
    }

    /// Topic files modified after the given timestamp (seconds since epoch).
    /// The dream gate calls this to check whether new material landed since
    /// the last dream. Delegates to the io helper so the enumeration stays
    /// one path.
    fn count_new_since(&self, since: u64) -> usize {
        count_new_since_impl(self, since)
    }

    /// Fetch the full body of one memory by key. Searches every root (newest
    /// mtime wins, matching recall) so a key living in a non-write scope
    /// still resolves. Returns None when the key is absent.
    fn show_memory(&self, key: &str) -> Option<MemoryEntry> {
        // Sanitize to the canonical NFC form so a requested key matches the
        // on-disk stem regardless of the caller's decomposition.
        let key = sanitize_key(key).ok()?;
        let topic = self.scan_candidates().into_iter().find(|t| t.key == key)?;
        self.read_topic(&topic.path).ok()
    }

    /// Fetch one memory body from the named scope's root only. Delegates to
    /// the io module so the per-root read stays beside the enumeration.
    fn show_memory_in_scope(&self, key: &str, scope: MemoryScope) -> Option<MemoryEntry> {
        show_memory_in_scope_impl(self, key, scope)
    }

    /// The scopes holding this key, in root order. Delegates to the io module;
    /// the merged scan cannot report a second copy of a key.
    fn scopes_for_key(&self, key: &str) -> Vec<MemoryScope> {
        scopes_for_key_impl(self, key)
    }

    /// The auto-scope write root (the last root). The consolidation dream
    /// locates the memory directory + places the lock here. A multi-root
    /// provider writes to the last root, so that is the canonical write path
    /// the dream consolidates.
    fn memory_root(&self) -> String {
        self.write_root().to_string_lossy().into_owned()
    }

    /// Delete a topic from the auto scope. Topic removal is authoritative;
    /// derived-index repair is best-effort and self-heals later.
    fn delete_memory(&self, key: &str) -> Result<(), MemoryError> {
        // Delegate to the scoped form so there is one delete path
        // (root_for_scope(Auto) is the write root).
        self.delete_memory_in_scope(key, MemoryScope::Auto)
    }

    /// Delete a topic from one scope, prune its global stats, and rebuild the
    /// derived indexes.
    fn delete_memory_in_scope(&self, key: &str, scope: MemoryScope) -> Result<(), MemoryError> {
        let key = sanitize_key(key)?;
        let topic_root = self.root_for_scope(scope).to_path_buf();
        let topic_path = topic_root.join(format!("{key}.md"));
        let _guard = self.write_lock.lock().expect("write lock poisoned");
        let _root_locks = self.lock_roots()?;
        // The marker goes down before the topic does. A delete whose index
        // rebuild then fails leaves the marker for the next session, rather
        // than a row naming a file already gone.
        let index_was_dirty = Self::index_marked_dirty(&topic_root);
        Self::mark_index_dirty(&topic_root)?;
        let removed = fs::remove_file(&topic_path);
        if removed.is_err() && !index_was_dirty {
            // Nothing changed, so the marker this call added would only cost
            // the next session a rebuild it does not need.
            Self::clear_index_dirty(&topic_root);
        }
        match removed {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Err(MemoryError::NotFound);
            }
            Err(_) => return Err(MemoryError::Io),
        }
        // Prune the stats sidecar entry so it does not grow monotonically
        // with deleted memories (advisory: a missed prune leaves a stale
        // stats row, not an invariant violation — load_stats treats a
        // corrupt sidecar as empty). Under the root locks so it cannot race
        // a concurrent rebuild on the same root.
        let mut stats = self.load_stats();
        if stats.remove(key.as_str()).is_some() {
            self.save_stats(&stats);
        }
        // Best-effort index rebuild: the topic is already gone, and recall
        // scans topic files (not the index), so a rebuild failure leaves only
        // cosmetic drift in MEMORY.md healed by the next rebuild_if_stale.
        // Surfacing the rebuild error to the caller would mislabel a
        // successful delete as a failure — a retry would then return NotFound
        // for a key whose topic is already removed.
        if let Err(e) = self.rebuild_index_impl_locked() {
            tracing::warn!("memory delete: index rebuild failed (heals next session): {e}");
        }
        Ok(())
    }

    /// Read the advisory recall-stats sidecar as typed records. A missing or
    /// corrupt sidecar yields an empty vec (cold restart). The dream reads
    /// this to nominate stale entries (low recall hits + old last access) for
    /// pruning — a 30-day-unrecalled memory is dead weight.
    fn read_recall_stats(&self) -> Vec<MemoryRecallStats> {
        self.load_stats()
            .into_iter()
            .map(|(key, r)| MemoryRecallStats {
                key,
                recall_hits: r.recall_hits,
                gate_violations: r.gate_violations,
                last_access_ts: r.last_access_ts,
            })
            .collect()
    }

    /// Record recall hits under the write lock so delete cannot leave orphan
    /// stats. Sidecar corruption degrades to empty counters. Lock and root are
    /// claimed without waiting: these counters are advisory, so a writer or
    /// healer busy elsewhere skips this update rather than stalling the turn.
    fn record_recall_hits(&self, keys: &[String]) {
        if keys.is_empty() {
            return;
        }
        let Ok(_guard) = self.write_lock.try_lock() else {
            tracing::debug!("memory recall stats write skipped: writer busy (advisory)");
            return;
        };
        let _root_lock = match RootLock::try_acquire(self.write_root()) {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                tracing::debug!("memory recall stats write skipped: root busy (advisory)");
                return;
            }
            Err(_) => {
                tracing::warn!("memory recall stats write skipped: root lock unavailable");
                return;
            }
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut stats = self.load_stats();
        for key in keys {
            let r = stats.entry(key.clone()).or_default();
            r.recall_hits = r.recall_hits.saturating_add(1);
            r.last_access_ts = now;
        }
        self.save_stats(&stats);
    }

    /// Record one gate violation under the write lock using the canonical key.
    /// Sidecar corruption degrades to empty counters. Lock and root are claimed
    /// without waiting: the counter is advisory and must not stall the gate.
    fn record_gate_violation(&self, key: &str) {
        let Ok(key) = sanitize_key(key) else {
            return;
        };
        let Ok(_guard) = self.write_lock.try_lock() else {
            tracing::debug!("memory gate stats write skipped: writer busy (advisory)");
            return;
        };
        let _root_lock = match RootLock::try_acquire(self.write_root()) {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                tracing::debug!("memory gate stats write skipped: root busy (advisory)");
                return;
            }
            Err(_) => {
                tracing::warn!("memory gate stats write skipped: root lock unavailable");
                return;
            }
        };
        let mut stats = self.load_stats();
        let r = stats.entry(key).or_default();
        r.gate_violations = r.gate_violations.saturating_add(1);
        self.save_stats(&stats);
    }

    fn rank_candidates(&self, query: &str, surfaced: &HashSet<String>) -> Vec<MemoryRankHit> {
        let keywords = tokenize(query);
        // A query with no word content ranks nothing. A query the tokenizer
        // cannot split (a single CJK char) still scans, so its rows reach
        // the semantic stage at score zero instead of being dropped.
        if keywords.is_empty() && !has_word_content(query) {
            return Vec::new();
        }
        let candidates = self.scan_candidates();
        // Filter already-surfaced keys BEFORE ranking so fresh candidates
        // beyond the scan cap get a chance when the top-ranked entries are
        // all surfaced. Surfaced is caller-provided (the set of keys already
        // in the assembled context), so the provider holds no surfaced state
        // across calls.
        let fresh: Vec<ScannedTopic> = candidates
            .into_iter()
            .take(SCAN_FILE_CAP)
            .filter(|t| !surfaced.contains(&t.key))
            .collect();
        // Frontmatter-only scan (no full bodies): score keyword overlap over
        // name plus description. Zero-score rows stay in the list, sorted
        // last, as the semantic stage's candidate input for a query the
        // lexical stage could not match.
        let mut hits: Vec<MemoryRankHit> = Vec::new();
        for t in fresh {
            let fm = match self.read_frontmatter(&t.path) {
                Ok(fm) => fm,
                Err(_) => continue,
            };
            let score = hit_count(&fm.name, &keywords) + hit_count(&fm.description, &keywords);
            hits.push(MemoryRankHit::new(
                t.key,
                fm.description,
                fm.source,
                MemoryScope::for_root_index(t.root_index),
                t.mtime,
                score,
            ));
        }
        // Relevance first, then recency (newest first).
        hits.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| b.mtime_secs.cmp(&a.mtime_secs))
        });
        hits
    }

    fn add(&self, entry: MemoryEntry) -> Result<(), MemoryError> {
        self.add_if_changed(entry).map(|_| ())
    }

    fn add_if_changed(&self, mut entry: MemoryEntry) -> Result<MemoryWriteOutcome, MemoryError> {
        let _guard = self.write_lock.lock().expect("write lock poisoned");
        entry.key = sanitize_key(&entry.key)?;
        validate_index_key(&entry.key)?;
        let root = self.write_root().to_path_buf();
        self.add_if_changed_in_root(&root, &entry)
    }

    /// Write a new memory entry into a specific storage scope. Used by the
    /// forked consolidation dream to refresh a project-scope entry in place
    /// rather than writing a competing auto-scope copy that would shadow the
    /// explicit version by newest-mtime (the dedup divergence the charter
    /// audit flagged). Closes the auto-shadows-explicit edge case.
    fn add_in_scope(&self, entry: MemoryEntry, scope: MemoryScope) -> Result<(), MemoryError> {
        self.add_in_scope_if_changed(entry, scope).map(|_| ())
    }

    fn add_in_scope_if_changed(
        &self,
        mut entry: MemoryEntry,
        scope: MemoryScope,
    ) -> Result<MemoryWriteOutcome, MemoryError> {
        let _guard = self.write_lock.lock().expect("write lock poisoned");
        entry.key = sanitize_key(&entry.key)?;
        validate_index_key(&entry.key)?;
        let root = self.root_for_scope(scope).to_path_buf();
        self.add_if_changed_in_root(&root, &entry)
    }

    /// Promote a topic into the project scope; idempotent.
    fn promote_memory(&self, key: &str) -> Result<(), MemoryError> {
        self.promote_impl(key)
    }

    /// Demote a topic back into the automatic scope; idempotent.
    fn demote_memory(&self, key: &str) -> Result<(), MemoryError> {
        self.demote_impl(key)
    }
}

impl MarkdownMemoryProvider {
    /// Compare and land one topic plus its index under the caller-held
    /// in-process lock and this root's cross-process lock.
    ///
    /// The topic is the source of truth, so a failed index write is reported
    /// as drift rather than undone: the next rebuild regenerates the index
    /// from the topic files. Removing or restoring the topic because derived
    /// data failed would delete a record the user just made.
    fn add_if_changed_in_root(
        &self,
        root: &Path,
        entry: &MemoryEntry,
    ) -> Result<MemoryWriteOutcome, MemoryError> {
        let _root_lock = RootLock::acquire(root)?;
        let topic_path = root.join(format!("{}.md", entry.key));
        let payload = serialize_topic_file(entry).into_bytes();
        let previous = match fs::read(&topic_path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == ErrorKind::NotFound => None,
            Err(_) => return Err(MemoryError::Io),
        };
        if previous.as_deref() == Some(payload.as_slice()) {
            return Ok(MemoryWriteOutcome::Unchanged);
        }
        let outcome = if previous.is_some() {
            MemoryWriteOutcome::Updated
        } else {
            MemoryWriteOutcome::Created
        };
        // A marker already present names an earlier index update this store
        // never published. This write appends only its own row, so it must
        // not clear that marker: the earlier gap would otherwise become
        // invisible to the next session's checks.
        let index_was_dirty = Self::index_marked_dirty(root);
        Self::mark_index_dirty(root)?;
        write_bytes_atomic(&topic_path, &payload)?;
        match self.append_index_pointer_in(root, entry) {
            Ok(()) if !index_was_dirty => Self::clear_index_dirty(root),
            Ok(()) => {}
            Err(error) => tracing::warn!(
                "memory index update failed for {}: {error} (heals on the next rebuild)",
                topic_path.display()
            ),
        }
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests;
