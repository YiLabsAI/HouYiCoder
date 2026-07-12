//! Markdown directory plus derived-index memory provider.
//!
//! A memory directory of markdown files with YAML frontmatter plus a
//! derived MEMORY index file. Three design decisions:
//!
//! 1. Deterministic recall: candidate files are ranked by keyword overlap
//!    over frontmatter fields, so the hot path pays no per-turn model
//!    side-query (no latency, no token cost per turn).
//! 2. Single-source atomic write: the topic file is the single source of
//!    truth; the index is a derived projection regenerated from topic
//!    files. write_atomic lands the topic file and its index pointer
//!    under a lock with a best-effort rollback if the index pointer cannot
//!    land. The rollback is best-effort: if the remove itself fails the
//!    store is left half-written and AtomicityFailed is returned. A write
//!    fans out to a single topic file rather than three independent paths,
//!    so a crash cannot leave the store half-written across paths.
//! 3. No render-path synchronous read: recall is invoked from the agent
//!    select step, never from a synchronous render path.
//!
//! File layout under root:
//!   <root>/<key>.md   topic record (frontmatter + body)
//!   <root>/MEMORY.md  derived index (pointer per topic, byte-capped)
//!
//! Topic record frontmatter fields: name, description, source.

use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use std::cmp::Reverse;

use crate::provider::{hit_count, tokenize};
use houyicoder_api::memory::MemoryProvider;
use houyicoder_context::{MemoryEntry, MemoryError, MemoryRecallStats, MemoryScope, MemorySummary};

mod io;
mod roots;
use io::{
    Frontmatter, INDEX_BYTE_CAP, INDEX_FILE, ScannedTopic, StatRecord, cap_index_content,
    count_new_since_impl, first_line, first_rule_sentence, list_memories_impl,
    merge_rule_into_carrier, parse_topic_file, sanitize_key, serialize_topic_file,
    strip_rule_from_carrier, write_bytes_atomic,
};

/// Cap on candidate files scanned per recall; keeps the scan bounded.
const SCAN_FILE_CAP: usize = 200;

/// Cap on files returned per recall (five ranked filenames).
const RECALL_RESULT_CAP: usize = 5;

/// Cap on lines in the derived MEMORY index so it never grows past a
/// bounded number of entries even with short lines.
const MAX_ENTRYPOINT_LINES: usize = 200;

/// Advisory recall-frequency sidecar filename. Lives in the write root
/// alongside the topic files + the derived index. Advisory: no lock, no
/// atomic write, no self-heal — a lost or corrupt sidecar is a cold
/// restart (counters re-accumulate), never an invariant violation.
const STATS_FILE: &str = ".stats.json";

/// The project memory file (always-on carrier) basename, written into the
/// workspace root by promote_memory and read at session start by the
/// project-context section.
const PROJECT_MEMORY_FILE: &str = "agent.md";

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
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
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
        candidates.sort_by_key(|c| Reverse(c.mtime));
        candidates
    }

    /// Scan every root and merge. When the same key appears in multiple
    /// scopes, the newest mtime wins so recall does not return duplicates.
    fn scan_candidates(&self) -> Vec<ScannedTopic> {
        let mut by_key: std::collections::HashMap<String, ScannedTopic> =
            std::collections::HashMap::new();
        for (root_index, root) in self.roots.iter().enumerate() {
            for mut t in Self::scan_root(root) {
                t.root_index = root_index;
                let keep = match by_key.get(&t.key) {
                    None => true,
                    Some(e) => t.mtime > e.mtime,
                };
                if keep {
                    by_key.insert(t.key.clone(), t);
                }
            }
        }
        let mut merged: Vec<ScannedTopic> = by_key.into_values().collect();
        merged.sort_by_key(|c| Reverse(c.mtime));
        merged
    }

    /// Rebuild the derived MEMORY index for every root from that root's own
    /// topic files. The index is a projection: it can always be regenerated
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
            std::io::ErrorKind::NotFound => MemoryError::NotFound,
            _ => MemoryError::Io,
        })?;
        let (fm, _body) = parse_topic_file(&text)?;
        Ok(fm)
    }

    /// Read the full topic record (frontmatter plus body) and project it
    /// into a MemoryEntry.
    fn read_topic(&self, path: &Path) -> Result<MemoryEntry, MemoryError> {
        let text = fs::read_to_string(path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => MemoryError::NotFound,
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
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Ok(MemoryEntry::new(key, body, fm.source).with_meta(fm.description.clone(), mtime_secs))
    }

    /// Append (or write) an index pointer for the entry in the given root.
    /// Inherent helper so add + add_in_scope share one append path.
    fn append_index_pointer_in(&self, root: &Path, entry: &MemoryEntry) -> Result<(), String> {
        let index_path = Self::index_path_for(root);
        let mut existing = fs::read_to_string(&index_path).unwrap_or_default();
        let line = format!(
            "- {} [{}]: {}\n",
            entry.key,
            entry.source.as_label(),
            first_line(&entry.content)
        );
        existing.push_str(&line);
        existing = cap_index_content(&existing, self.max_lines);
        write_bytes_atomic(&index_path, existing.as_bytes()).map_err(|e| e.to_string())
    }

    /// The path to the project memory file (always-on carrier). Derived
    /// from the project scope root: the project root sits two directories
    /// below the workspace root, so the workspace root is two parents up.
    /// Returns None when there is no project root or the path cannot
    /// resolve (a single-root provider has no project scope, so promote /
    /// demote degrade to a no-op on the carrier file + a topic move within
    /// the one root, which is a safe no-op).
    fn project_memory_file_path(&self) -> Option<PathBuf> {
        let project_root = self.roots.get(1)?;
        let workspace = project_root.parent()?.parent()?;
        Some(workspace.join(PROJECT_MEMORY_FILE))
    }
}

impl MemoryProvider for MarkdownMemoryProvider {
    /// Regenerate the derived index from all topic files (full rebuild,
    /// self-healing projection).
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

    /// The auto-scope write root (the last root). The consolidation dream
    /// locates the memory directory + places the lock here. A multi-root
    /// provider writes to the last root, so that is the canonical write path
    /// the dream consolidates.
    fn memory_root(&self) -> String {
        self.write_root().to_string_lossy().into_owned()
    }

    /// Delete one topic by key from the write root. Removes the topic file
    /// under the write lock (so a concurrent add of the same key cannot
    /// race), then regenerates the derived index so the deleted pointer
    /// disappears. The index rebuild runs after the guard drops because the
    /// rebuild takes the same lock and the lock is not reentrant. Best-effort
    /// on the rebuild: a failure still leaves the topic gone — recall scans
    /// topic files, not the index, so a stale pointer is cosmetic drift
    /// healed by the next rebuild_if_stale. This form deletes from the auto
    /// scope (the dream's consolidation target).
    fn delete_memory(&self, key: &str) -> Result<(), MemoryError> {
        // Delegate to the scoped form so there is one delete path
        // (root_for_scope(Auto) is the write root).
        self.delete_memory_in_scope(key, MemoryScope::Auto)
    }

    /// Delete by key from a specific scope's root. The /memory pane calls
    /// this so forget on a user/project row deletes the file in that scope's
    /// root, not just the auto-scope copy (which would leave the explicit
    /// original and the list would still show it). The stats sidecar (auto
    /// root) and the index rebuild (all roots) are scope-wide, so a delete in
    /// any scope prunes the global stats entry + regenerates every root's
    /// index.
    fn delete_memory_in_scope(&self, key: &str, scope: MemoryScope) -> Result<(), MemoryError> {
        let key = sanitize_key(key)?;
        let topic_path = self.root_for_scope(scope).join(format!("{key}.md"));
        {
            let _guard = self.write_lock.lock().expect("write lock poisoned");
            match fs::remove_file(&topic_path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
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

    /// Increment recall_hits + update last_access_ts for the keys a recall
    /// just surfaced. Under the write lock so a concurrent delete cannot
    /// prune a key this increment is about to re-create (an orphan stats
    /// row for a removed topic). The lock is cheap off the hot path — recall
    /// fires at turn entry, not per model call. A crash mid-write still
    /// leaves a corrupt sidecar the next load treats as empty (cold
    /// restart). gate_violations is untouched here (fed by the PreToolUse
    /// gate in a later sprint).
    fn record_recall_hits(&self, keys: &[String]) {
        if keys.is_empty() {
            return;
        }
        let _guard = self.write_lock.lock().expect("write lock poisoned");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
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

    /// Increment gate_violations for one key (signal B: a PreToolUse gate
    /// denied a call because the agent violated the rule that key names).
    /// Under the write lock so a concurrent delete cannot prune a key this
    /// increment is about to re-create. The key is sanitized to the
    /// canonical NFC form so a caller passing a deny reason that happens
    /// to name the rule still maps to the on-disk stem. A crash mid-write
    /// leaves a corrupt sidecar the next load treats as empty (cold
    /// restart). recall_hits + last_access_ts are untouched (fed by
    /// record_recall_hits on the recall path).
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

    fn recall(&self, query: &str, budget: usize, surfaced: &HashSet<String>) -> Vec<MemoryEntry> {
        if budget == 0 {
            return Vec::new();
        }
        let keywords = tokenize(query);
        if keywords.is_empty() {
            return Vec::new();
        }
        let candidates = self.scan_candidates();
        // Filter already-surfaced keys BEFORE ranking so fresh candidates
        // beyond the result cap get a chance when the top-ranked entries are
        // all surfaced. Surfaced is caller-provided (the set of keys already
        // in the served view), so the provider holds no surfaced state
        // across calls.
        let fresh: Vec<ScannedTopic> = candidates
            .into_iter()
            .take(SCAN_FILE_CAP)
            .filter(|t| !surfaced.contains(&t.key))
            .collect();
        // Phase one: frontmatter-only scan (do not read full bodies yet).
        // Rank by keyword overlap over name plus description.
        let mut ranked: Vec<(u32, u64, ScannedTopic)> = Vec::new();
        for t in fresh.iter() {
            let fm = match self.read_frontmatter(&t.path) {
                Ok(fm) => fm,
                Err(_) => continue,
            };
            let hits = hit_count(&fm.name, &keywords) + hit_count(&fm.description, &keywords);
            if hits == 0 {
                continue;
            }
            ranked.push((hits, t.mtime, t.clone()));
        }
        // Relevance first, then recency (newest first).
        ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
        ranked.truncate(RECALL_RESULT_CAP);

        // Phase two: load full bodies of the selected files only, budget pack.
        let mut used: usize = 0;
        let mut out = Vec::new();
        for (_, _, t) in ranked.into_iter() {
            let entry = match self.read_topic(&t.path) {
                Ok(e) => e,
                Err(_) => continue,
            };
            if used + entry.tokens > budget {
                // Stop at the first entry that would overflow the budget
                // (rank order matters; do not skip ahead).
                break;
            }
            used += entry.tokens;
            out.push(entry);
        }
        out
    }

    fn add(&self, mut entry: MemoryEntry) -> Result<(), MemoryError> {
        let _guard = self.write_lock.lock().expect("write lock poisoned");
        // Canonicalize the key to NFC so the filename, frontmatter name, and
        // index pointer all live under one form — a caller passing a
        // decomposed grapheme still writes the precomposed file.
        entry.key = sanitize_key(&entry.key)?;
        let root = self.write_root().to_path_buf();
        self.add_in_root(&root, &entry)
    }

    /// Write a new memory entry into a specific storage scope. Used by the
    /// forked consolidation dream to refresh a project-scope entry in place
    /// rather than writing a competing auto-scope copy that would shadow the
    /// explicit version by newest-mtime (the dedup divergence the charter
    /// audit flagged). Closes the auto-shadows-explicit edge case.
    fn add_in_scope(&self, mut entry: MemoryEntry, scope: MemoryScope) -> Result<(), MemoryError> {
        let _guard = self.write_lock.lock().expect("write lock poisoned");
        entry.key = sanitize_key(&entry.key)?;
        let root = self.root_for_scope(scope).to_path_buf();
        self.add_in_root(&root, &entry)
    }

    /// Promote a topic from the auto scope into the project scope (the
    /// always-on carrier). Lands the three-step file-op sequence the design
    /// pins: (1) merge the topic's rule sentence (the first content line,
    /// the rule itself) into the project memory file (agent.md), creating
    /// the file if missing, skipping the merge when the sentence is already
    /// present; (2) move the topic file from the auto root into the
    /// project root so recall still finds it under the project scope; (3)
    /// regenerate the derived indexes for both roots. Idempotent: a topic
    /// already living in the project root only merges the rule sentence.
    /// Returns NotFound when the topic is not in the auto root.
    fn promote_memory(&self, key: &str) -> Result<(), MemoryError> {
        let key = sanitize_key(key)?;
        // Hold the write lock across the read-merge-move AND the rebuild so
        // a concurrent add cannot land a competing auto copy between the
        // move and the index regeneration (TOCTOU). rebuild_index_impl_locked
        // assumes the lock is held; the public rebuild_index_impl takes its
        // own lock and would deadlock nested.
        let _guard = self.write_lock.lock().expect("write lock poisoned");
        let auto_root = self.write_root().to_path_buf();
        let project_root = self.root_for_scope(MemoryScope::Project).to_path_buf();
        let src_topic = auto_root.join(format!("{key}.md"));
        let dst_topic = project_root.join(format!("{key}.md"));
        let exists_in_auto = src_topic.is_file();
        let exists_in_project = dst_topic.is_file();
        // Read the rule sentence before any move so a missing topic is
        // detected before any file is touched. Prefer the project copy
        // when both exist (the explicit scope is the source of truth);
        // the rule sentence is the first non-empty line of the body.
        let rule_sentence = if exists_in_project {
            let entry = self.read_topic(&dst_topic)?;
            first_rule_sentence(&entry.content)
        } else if exists_in_auto {
            let entry = self.read_topic(&src_topic)?;
            first_rule_sentence(&entry.content)
        } else {
            return Err(MemoryError::NotFound);
        };
        // Step one: merge the rule sentence into the project memory file.
        // Best-effort: a write failure leaves the carrier file unchanged
        // but the topic move still lands.
        if let Some(carrier) = self.project_memory_file_path()
            && let Err(e) = merge_rule_into_carrier(&carrier, &rule_sentence)
        {
            tracing::warn!("memory promote: carrier merge failed (continuing): {e}");
        }
        // Step two: reconcile the topic files across the two roots. When
        // both roots have a copy, the project copy is the explicit source
        // of truth — remove the auto copy so it cannot shadow the project
        // one by newest-mtime (the MED-1 divergence). When only the auto
        // copy exists, move it auto -> project. When only the project copy
        // exists, the move is a no-op (idempotent promote).
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
        // Step three: regenerate both indexes under the same lock so the
        // auto root loses the pointer and the project root gains it.
        drop(self.rebuild_index_impl_locked());
        Ok(())
    }

    /// Demote a topic from the project scope back into the auto scope. The
    /// reverse of promote: (1) remove the rule sentence from the project
    /// memory file (agent.md); (2) move the topic file from the project
    /// root into the auto root so the topic is recall-on-demand only;
    /// (3) regenerate both indexes. Idempotent: a topic already in the
    /// auto root only removes the carrier line. Returns NotFound when the
    /// topic is in neither root.
    fn demote_memory(&self, key: &str) -> Result<(), MemoryError> {
        let key = sanitize_key(key)?;
        // Hold the write lock across the read-strip-move AND the rebuild
        // (same TOCTOU closure as promote_memory). rebuild_index_impl_locked
        // assumes the lock is held.
        let _guard = self.write_lock.lock().expect("write lock poisoned");
        let auto_root = self.write_root().to_path_buf();
        let project_root = self.root_for_scope(MemoryScope::Project).to_path_buf();
        let src_topic = project_root.join(format!("{key}.md"));
        let dst_topic = auto_root.join(format!("{key}.md"));
        let exists_in_project = src_topic.is_file();
        let exists_in_auto = dst_topic.is_file();
        // Read the rule sentence before any move. Prefer the project copy
        // when both exist (it carries the rule sentence promote added).
        let rule_sentence = if exists_in_project {
            let entry = self.read_topic(&src_topic)?;
            first_rule_sentence(&entry.content)
        } else if exists_in_auto {
            let entry = self.read_topic(&dst_topic)?;
            first_rule_sentence(&entry.content)
        } else {
            return Err(MemoryError::NotFound);
        };
        // Step one: strip the rule sentence from the project memory file.
        if let Some(carrier) = self.project_memory_file_path()
            && let Err(e) = strip_rule_from_carrier(&carrier, &rule_sentence)
        {
            tracing::warn!("memory demote: carrier strip failed (continuing): {e}");
        }
        // Step two: reconcile the topic files. When both roots have a copy,
        // the auto copy is the recall-on-demand target — remove the project
        // copy so the always-on scope no longer carries the topic. When only
        // the project copy exists, move it project -> auto. When only the
        // auto copy exists, the move is a no-op (idempotent demote).
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
        // Step three: regenerate both indexes under the same lock.
        drop(self.rebuild_index_impl_locked());
        Ok(())
    }
}

impl MarkdownMemoryProvider {
    /// Land a topic file plus its index pointer in a specific root under the
    /// write lock. Shared by add (auto root) and add_in_scope (any root).
    /// The caller holds the write lock + has sanitized the key.
    fn add_in_root(&self, root: &Path, entry: &MemoryEntry) -> Result<(), MemoryError> {
        fs::create_dir_all(root).map_err(|_| MemoryError::Io)?;
        let topic_path = root.join(format!("{}.md", entry.key));
        let payload = serialize_topic_file(entry);
        // Step one: land the topic file atomically (temp plus rename).
        write_bytes_atomic(&topic_path, payload.as_bytes())?;
        // Step two: append a pointer to the derived index. If this fails,
        // attempt a best-effort rollback by removing the topic file. If the
        // rollback remove itself fails the store is left half-written (topic
        // file present, index pointer missing) and AtomicityFailed is
        // returned so the caller knows the store is inconsistent.
        if let Err(e) = self.append_index_pointer_in(root, entry) {
            if let Err(rm_err) = fs::remove_file(&topic_path) {
                tracing::warn!(
                    "memory rollback: failed to remove topic file {}: {rm_err}",
                    topic_path.display()
                );
            }
            return Err(MemoryError::AtomicityFailed(format!(
                "index pointer failed: {e}"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "markdown_memory_tests.rs"]
mod markdown_memory_tests;

#[cfg(test)]
#[path = "markdown_memory_delete_tests.rs"]
mod markdown_memory_delete_tests;

#[cfg(test)]
#[path = "markdown_memory_scope_tests.rs"]
mod markdown_memory_scope_tests;

#[cfg(test)]
#[path = "markdown_memory_scope_flow_tests.rs"]
mod markdown_memory_scope_flow_tests;
