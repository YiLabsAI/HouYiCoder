//! Markdown directory plus derived-index memory provider.
//!
//! Topic files are the source of truth; MEMORY.md is a bounded derived index.
//! Recall ranks frontmatter by keyword overlap and reads selected bodies only.
//! Writes compare and replace a topic under one lock, then reconcile the index.
//! Failed index updates restore the previous topic when possible. Rendering
//! never performs synchronous reads.
//!
//! Each root contains key.md topic records and a derived MEMORY.md index.

use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use std::cmp::Reverse;

use crate::provider::{has_word_content, hit_count, tokenize};
use houyicoder_api::memory::{MemoryProvider, MemoryWriteOutcome};
use houyicoder_context::{
    MemoryEntry, MemoryError, MemoryRankHit, MemoryRecallStats, MemoryScope, MemorySummary,
};

mod io;
mod roots;
use io::{
    Frontmatter, INDEX_BYTE_CAP, INDEX_FILE, ScannedTopic, StatRecord, cap_index_content,
    count_new_since_impl, first_line, list_memories_impl, parse_topic_file, sanitize_key,
    scopes_for_key_impl, serialize_topic_file, show_memory_in_scope_impl, write_bytes_atomic,
};
use std::io::ErrorKind;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// Cap on candidate files scanned per rank; keeps the scan bounded.
const SCAN_FILE_CAP: usize = 200;

/// Cap on lines in the derived MEMORY index so it never grows past a
/// bounded number of entries even with short lines.
const MAX_ENTRYPOINT_LINES: usize = 200;

/// Advisory recall-frequency sidecar filename. Lives in the write root
/// alongside the topic files + the derived index. Advisory: no lock, no
/// atomic write, no self-heal — a lost or corrupt sidecar is a cold
/// restart (counters re-accumulate), never an invariant violation.
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
    /// Empty roots are dropped; duplicate paths are deduped (first wins, so
    /// the higher-priority scope is kept) — a degenerate config where two
    /// roots resolve to the same path would otherwise scan it twice. A
    /// missing root is treated as no candidates (created on demand at
    /// write time only for the write root).
    pub fn new_multi(roots: Vec<PathBuf>) -> Self {
        let mut seen: HashSet<PathBuf> = HashSet::new();
        let roots: Vec<PathBuf> = roots
            .into_iter()
            .filter(|r| !r.as_os_str().is_empty())
            .filter(|r| seen.insert(r.clone()))
            .collect();
        Self::with_max_lines(roots, MAX_ENTRYPOINT_LINES)
    }

    /// Construct with a custom index line cap. Tests use a small value so the
    /// line-cap path is exercised without hundreds of file writes.
    pub fn with_max_lines(roots: Vec<PathBuf>, max_lines: usize) -> Self {
        let roots = if roots.is_empty() {
            vec![PathBuf::new()]
        } else {
            roots
        };
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

    /// Persist the stats map best-effort. No lock, no atomic write — a crash
    /// mid-write leaves a corrupt sidecar the next load treats as empty
    /// (cold restart). Advisory by design.
    fn save_stats(&self, stats: &HashMap<String, StatRecord>) {
        let Ok(json) = serde_json::to_string(stats) else {
            return;
        };
        drop(fs::write(self.stats_path(), json));
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
                        (rank, t.mtime) < (existing_rank, e.mtime)
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
        self.rebuild_index_impl_locked()
    }

    /// rebuild_index_impl assumed the caller already holds the write lock.
    /// Used by promote/demote so the rebuild runs under the SAME lock as
    /// the topic move — closing the TOCTOU window where a concurrent add
    /// could land a competing copy between the move and the rebuild.
    fn rebuild_index_impl_locked(&self) -> Result<(), MemoryError> {
        for root in &self.roots {
            fs::create_dir_all(root).map_err(|_| MemoryError::Io)?;
            let topics = Self::scan_root(root);
            let mut out = String::from("# Memory index\n\n");
            for t in topics.iter().take(SCAN_FILE_CAP) {
                let line = match self.read_frontmatter(&t.path) {
                    Ok(fm) => format!(
                        "- {} [{}]: {}\n",
                        fm.name,
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

/// Stamp a file as freshly modified. Promotion is an edit of the record's
/// standing, so the file it lands in carries the newest time; a failure is
/// only a lost tie-break, never a lost write.
fn touch_file(path: &Path) {
    if let Ok(file) = OpenOptions::new().append(true).open(path) {
        drop(file.set_modified(SystemTime::now()));
    }
}

impl MemoryProvider for MarkdownMemoryProvider {
    /// Regenerate the derived index from all topic files (a full,
    /// self-healing rebuild).
    fn rebuild_index(&self) -> Result<(), MemoryError> {
        self.rebuild_index_impl()
    }

    /// Rebuild only when a root's index is stale (a topic newer than the
    /// index, or the index missing). Run on session start + file-changed so a
    /// crash or external edit cannot leave a drifted index across runs.
    fn rebuild_if_stale(&self) -> Result<(), MemoryError> {
        if self.roots.iter().any(|r| Self::root_is_stale(r)) {
            self.rebuild_index_impl()?;
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
        let topic_path = self.root_for_scope(scope).join(format!("{key}.md"));
        {
            let _guard = self.write_lock.lock().expect("write lock poisoned");
            match fs::remove_file(&topic_path) {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::NotFound => {
                    return Err(MemoryError::NotFound);
                }
                Err(_) => return Err(MemoryError::Io),
            }
            // Prune the stats sidecar entry so it does not grow monotonically
            // with deleted memories (advisory: a missed prune leaves a stale
            // stats row, not an invariant violation — load_stats treats a
            // corrupt sidecar as empty). Under the write lock so it cannot
            // race a concurrent rebuild_index_impl on the same root.
            let mut stats = self.load_stats();
            if stats.remove(key.as_str()).is_some() {
                self.save_stats(&stats);
            }
        }
        // Best-effort index rebuild: the topic is already gone, and recall
        // scans topic files (not the index), so a rebuild failure leaves only
        // cosmetic drift in MEMORY.md healed by the next rebuild_if_stale.
        // Surfacing the rebuild error to the caller would mislabel a
        // successful delete as a failure — a retry would then return NotFound
        // for a key whose topic is already removed.
        if let Err(e) = self.rebuild_index_impl() {
            tracing::warn!("memory delete: index rebuild failed (self-heals next session): {e}");
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
    /// stats. Sidecar corruption degrades to empty counters.
    fn record_recall_hits(&self, keys: &[String]) {
        if keys.is_empty() {
            return;
        }
        let _guard = self.write_lock.lock().expect("write lock poisoned");
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
    /// Sidecar corruption degrades to empty counters.
    fn record_gate_violation(&self, key: &str) {
        let Ok(key) = sanitize_key(key) else {
            return;
        };
        let _guard = self.write_lock.lock().expect("write lock poisoned");
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
        let root = self.root_for_scope(scope).to_path_buf();
        self.add_if_changed_in_root(&root, &entry)
    }

    /// Promote a topic into the project scope, then rebuild both derived
    /// indexes. Repeated promotion is idempotent.
    fn promote_memory(&self, key: &str) -> Result<(), MemoryError> {
        let key = sanitize_key(key)?;
        // Keep topic and index changes in one critical section.
        let _guard = self.write_lock.lock().expect("write lock poisoned");
        let auto_root = self.write_root().to_path_buf();
        let project_root = self.root_for_scope(MemoryScope::Project).to_path_buf();
        let src_topic = auto_root.join(format!("{key}.md"));
        let dst_topic = project_root.join(format!("{key}.md"));
        let exists_in_auto = src_topic.is_file();
        let exists_in_project = dst_topic.is_file();
        if !exists_in_auto && !exists_in_project {
            return Err(MemoryError::NotFound);
        }
        // Reconcile the topic files across the two roots. When both roots
        // have a copy, the project copy is the explicit source of truth —
        // remove the auto copy so the two cannot disagree. When only the
        // auto copy exists, move it auto -> project. When only the project
        // copy exists, the move is a no-op (idempotent promote).
        fs::create_dir_all(&project_root).map_err(|_| MemoryError::Io)?;
        if exists_in_auto && exists_in_project {
            // Both present: drop the auto copy, keep the project one.
            drop(fs::remove_file(&src_topic));
        } else if exists_in_auto {
            // Auto only: move auto -> project.
            if let Err(_e) = fs::rename(&src_topic, &dst_topic) {
                if fs::copy(&src_topic, &dst_topic).is_err() {
                    return Err(MemoryError::Io);
                }
                drop(fs::remove_file(&src_topic));
            }
        }
        // A promoted topic is explicit from now on. Its modification time is
        // stamped so the promotion itself is the newest edit, which keeps the
        // project copy ahead of any auto copy a later dream would write.
        touch_file(&dst_topic);
        // Step two: regenerate both indexes under the same lock so the
        // auto root loses the pointer and the project root gains it.
        drop(self.rebuild_index_impl_locked());
        Ok(())
    }

    /// Demote a topic into the auto scope and rebuild both derived indexes.
    /// Repeated demotion is idempotent.
    fn demote_memory(&self, key: &str) -> Result<(), MemoryError> {
        let key = sanitize_key(key)?;
        // Keep topic and index changes in one critical section.
        let _guard = self.write_lock.lock().expect("write lock poisoned");
        let auto_root = self.write_root().to_path_buf();
        let project_root = self.root_for_scope(MemoryScope::Project).to_path_buf();
        let src_topic = project_root.join(format!("{key}.md"));
        let dst_topic = auto_root.join(format!("{key}.md"));
        let exists_in_project = src_topic.is_file();
        let exists_in_auto = dst_topic.is_file();
        if !exists_in_project && !exists_in_auto {
            return Err(MemoryError::NotFound);
        }
        // Reconcile the topic files. When both roots have a copy, the auto
        // copy is the recall-on-demand target — remove the project copy so
        // the always-on scope no longer carries the topic. When only the
        // project copy exists, move it project -> auto. When only the auto
        // copy exists, the move is a no-op (idempotent demote).
        fs::create_dir_all(&auto_root).map_err(|_| MemoryError::Io)?;
        if exists_in_project && exists_in_auto {
            drop(fs::remove_file(&src_topic));
        } else if exists_in_project && fs::rename(&src_topic, &dst_topic).is_err() {
            // rename across directory boundaries on the same filesystem is
            // atomic; a cross-device failure falls back to copy-then-remove.
            if fs::copy(&src_topic, &dst_topic).is_err() {
                return Err(MemoryError::Io);
            }
            drop(fs::remove_file(&src_topic));
        }
        // Step two: regenerate both indexes under the same lock.
        drop(self.rebuild_index_impl_locked());
        Ok(())
    }
}

impl MarkdownMemoryProvider {
    /// Compare and land one topic plus its index under the caller-held lock.
    fn add_if_changed_in_root(
        &self,
        root: &Path,
        entry: &MemoryEntry,
    ) -> Result<MemoryWriteOutcome, MemoryError> {
        fs::create_dir_all(root).map_err(|_| MemoryError::Io)?;
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
        write_bytes_atomic(&topic_path, &payload)?;
        if let Err(error) = self.append_index_pointer_in(root, entry) {
            let rollback = match previous {
                Some(bytes) => write_bytes_atomic(&topic_path, &bytes),
                None => fs::remove_file(&topic_path).map_err(|_| MemoryError::Io),
            };
            if let Err(rollback_error) = rollback {
                tracing::warn!(
                    "memory rollback failed for {}: {rollback_error}",
                    topic_path.display()
                );
            }
            return Err(MemoryError::AtomicityFailed(format!(
                "index pointer failed: {error}"
            )));
        }
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests;
