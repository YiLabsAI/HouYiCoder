//! Tests for the markdown memory provider. Extracted to keep the main
//! module under the file-size gate.
//!
//! These tests cover recall ranking, de-dup placement, atomic write with
//! rollback, index line+byte caps, and boundary conditions.

use super::*;
use houyicoder_context::{MemoryOrigin, MemorySource};

fn temp_root() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("markdown_memory_test_{seq}_{}", std::process::id(),));
    std::fs::create_dir_all(&dir).expect("create temp root");
    dir
}

fn entry(key: &str, content: &str, source: MemorySource) -> MemoryEntry {
    MemoryEntry::new(key, content, source)
}

#[test]
fn test_recall_returns_within_budget() {
    let root = temp_root();
    let p = MarkdownMemoryProvider::new(root.clone());
    p.add(entry(
        "fox-facts",
        "the quick brown fox",
        MemorySource::Project,
    ))
    .unwrap();
    p.add(entry("cat-facts", "a sleepy cat naps", MemorySource::User))
        .unwrap();
    // "the quick brown fox" is 19 chars -> 5 tokens. Budget 5 admits
    // exactly one entry (the fox one ranks first for a fox query).
    let out = p.recall("fox", 5, &HashSet::new());
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].key, "fox-facts");
    assert!(out[0].tokens <= 5);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn test_recall_dedups_already_surfaced() {
    let root = temp_root();
    let p = MarkdownMemoryProvider::new(root.clone());
    p.add(entry(
        "fox-facts",
        "the quick brown fox here",
        MemorySource::Project,
    ))
    .unwrap();
    // First recall with an empty surfaced set returns the entry; the caller
    // records its key as surfaced for the next call.
    let first = p.recall("fox", 100, &HashSet::new());
    assert_eq!(first.len(), 1);
    let mut surfaced: HashSet<String> = first.iter().map(|e| e.key.clone()).collect();
    // Second recall with the surfaced set must not return the entry.
    let second = p.recall("fox", 100, &surfaced);
    assert!(second.is_empty(), "a surfaced entry must be skipped");
    // After a compaction-style reset (empty surfaced set), the entry is
    // eligible again — the natural reset the projection scan produces.
    surfaced.clear();
    let third = p.recall("fox", 100, &surfaced);
    assert_eq!(third.len(), 1);
    std::fs::remove_dir_all(&root).ok();
}

/// A key in the surfaced set passed to recall suppresses that entry; clearing
/// the set re-surfaces it. Pins the caller-driven surfaced contract that
/// replaces the old in-provider mutable de-dup seam.
#[test]
fn test_surfaced_param_suppresses() {
    let root = temp_root();
    let p = MarkdownMemoryProvider::new(root.clone());
    p.add(entry(
        "fox-facts",
        "the quick brown fox",
        MemorySource::Project,
    ))
    .unwrap();
    // surfaced = {fox-facts}: recall skips it.
    let mut surfaced = HashSet::new();
    surfaced.insert("fox-facts".to_string());
    assert!(
        p.recall("fox", 100, &surfaced).is_empty(),
        "surfaced key must be suppressed"
    );
    // Empty surfaced set: fox re-surfaces.
    surfaced.clear();
    assert_eq!(
        p.recall("fox", 100, &surfaced).len(),
        1,
        "cleared surfaced set must re-surface the entry"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn test_atomic_write_lands_topic() {
    let root = temp_root();
    let p = MarkdownMemoryProvider::new(root.clone());
    p.add(entry("alpha", "alpha fox body", MemorySource::User))
        .unwrap();
    let topic = root.join("alpha.md");
    let index = root.join(INDEX_FILE);
    assert!(topic.exists(), "topic file must land");
    assert!(index.exists(), "index pointer must land");
    let idx = fs::read_to_string(&index).unwrap();
    assert!(idx.contains("alpha"), "index must reference the key");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn test_add_rollback_bad() {
    let root = temp_root();
    let p = MarkdownMemoryProvider::new(root.clone());
    // A traversal key is rejected before any file lands.
    let res = p.add(entry("../escape", "bad", MemorySource::User));
    assert!(matches!(res, Err(MemoryError::InvalidPath(_))));
    assert!(!root.join("../escape.md").exists());
    // The index must not exist either (nothing landed).
    assert!(!root.join(INDEX_FILE).exists());
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn test_path_validator_rejects_bad() {
    assert!(matches!(sanitize_key(""), Err(MemoryError::InvalidPath(_))));
    assert!(matches!(
        sanitize_key(".."),
        Err(MemoryError::InvalidPath(_))
    ));
    assert!(matches!(
        sanitize_key("."),
        Err(MemoryError::InvalidPath(_))
    ));
    assert!(matches!(
        sanitize_key(".hidden"),
        Err(MemoryError::InvalidPath(_))
    ));
    assert!(matches!(
        sanitize_key("a/b"),
        Err(MemoryError::InvalidPath(_))
    ));
    assert!(matches!(
        sanitize_key("a\\b"),
        Err(MemoryError::InvalidPath(_))
    ));
    assert!(matches!(
        sanitize_key("C:exploit"),
        Err(MemoryError::InvalidPath(_))
    ));
    assert!(matches!(
        sanitize_key("x\0y"),
        Err(MemoryError::InvalidPath(_))
    ));
    assert!(matches!(
        sanitize_key("MEMORY"),
        Err(MemoryError::InvalidPath(_))
    ));
    // A clean key is accepted.
    assert!(sanitize_key("good-key").is_ok());
}

/// A control character in the key is rejected: a newline would split the
/// MEMORY.md pointer line mid-entry and the spillover reads as a second
/// valid pointer (index poisoning). Tab and the 0x01-0x1F range too.
#[test]
fn test_sanitize_rejects_control_chars() {
    assert!(matches!(
        sanitize_key("foo\nbar"),
        Err(MemoryError::InvalidPath(_))
    ));
    assert!(matches!(
        sanitize_key("foo\rbar"),
        Err(MemoryError::InvalidPath(_))
    ));
    assert!(matches!(
        sanitize_key("foo\tbar"),
        Err(MemoryError::InvalidPath(_))
    ));
    assert!(matches!(
        sanitize_key("foo\u{1f}bar"),
        Err(MemoryError::InvalidPath(_))
    ));
    assert!(matches!(
        sanitize_key("foo\u{7f}"),
        Err(MemoryError::InvalidPath(_))
    ));
}

/// A decomposed (NFD) key normalizes to its precomposed (NFC) form so
/// both map to one file. A key that is already NFC passes through
/// unchanged.
#[test]
fn test_sanitize_nfc_normalizes() {
    // "é" as e + combining acute (NFD: U+0065 U+0301).
    let nfd = "caf\u{65}\u{301}-rules";
    let out = sanitize_key(nfd).expect("nfd key sanitizes");
    assert_eq!(out, "café-rules", "nfd input collapses to nfc");
    // Already-NFC input is stable.
    let nfc = "café-rules";
    assert_eq!(sanitize_key(nfc).expect("nfc ok"), "café-rules");
    // The on-disk stem of an added NFD-key entry is NFC, so a recall with
    // the NFC key finds it (no NFD/NFC dedup divergence).
    let root = temp_root();
    let p = MarkdownMemoryProvider::new(root.clone());
    p.add(entry(nfd, "body", MemorySource::User)).unwrap();
    let found = p
        .show_memory("café-rules")
        .expect("nfc key finds nfd-written entry");
    assert_eq!(found.key, "café-rules", "stored key is the nfc form");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn test_rebuild_index_regenerates() {
    let root = temp_root();
    let p = MarkdownMemoryProvider::new(root.clone());
    p.add(entry("alpha", "alpha fox body", MemorySource::User))
        .unwrap();
    // Wipe the index; rebuild must restore it from topic files only.
    fs::remove_file(root.join(INDEX_FILE)).unwrap();
    p.rebuild_index().unwrap();
    let idx = fs::read_to_string(root.join(INDEX_FILE)).unwrap();
    assert!(idx.contains("alpha"), "rebuild must restore the pointer");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn test_recall_empty_query_returns() {
    let root = temp_root();
    let p = MarkdownMemoryProvider::new(root.clone());
    p.add(entry("fox", "fox body", MemorySource::Project))
        .unwrap();
    assert!(p.recall("", 100, &HashSet::new()).is_empty());
    assert!(p.recall("   ", 100, &HashSet::new()).is_empty());
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn test_recall_empty_root_returns() {
    let root = temp_root().join("nonexistent_subdir");
    let p = MarkdownMemoryProvider::new(root.clone());
    assert!(p.recall("anything", 100, &HashSet::new()).is_empty());
}

#[test]
fn test_round_trip_preserves_fields() {
    let root = temp_root();
    let p = MarkdownMemoryProvider::new(root.clone());
    let src = MemorySource::Feedback;
    p.add(entry("round-trip", "the fox feedback here", src))
        .unwrap();
    let out = p.recall("fox", 100, &HashSet::new());
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].key, "round-trip");
    assert_eq!(out[0].source, src);
    assert_eq!(out[0].content, "the fox feedback here");
    std::fs::remove_dir_all(&root).ok();
}

/// Successive recalls with an accumulating surfaced set surface different
/// entries until exhausted: the second call skips what the first returned and
/// returns the next match; once all matches are surfaced, recall is empty.
/// Pins the caller-driven surfaced contract at the provider boundary.
#[test]
fn test_surfaced_set_exhausts() {
    let root = temp_root();
    let p = MarkdownMemoryProvider::new(root.clone());
    p.add(entry("fox-a", "alpha fox", MemorySource::Project))
        .unwrap();
    p.add(entry("fox-b", "bravo fox", MemorySource::Project))
        .unwrap();
    let mut surfaced = HashSet::new();
    // First recall surfaces one of the two fox entries.
    let first = p.recall("fox", 4, &surfaced);
    assert_eq!(first.len(), 1);
    surfaced.extend(first.iter().map(|e| e.key.clone()));
    // Second recall skips the surfaced one and returns the other.
    let second = p.recall("fox", 4, &surfaced);
    assert_eq!(second.len(), 1, "the non-surfaced match must still surface");
    assert!(
        !surfaced.contains(&second[0].key),
        "second recall must not re-surface the first entry"
    );
    surfaced.extend(second.iter().map(|e| e.key.clone()));
    // Both surfaced: recall is empty.
    assert!(
        p.recall("fox", 4, &surfaced).is_empty(),
        "all matches surfaced must return empty"
    );
    // Clear the set: an entry surfaces again.
    surfaced.clear();
    assert_eq!(p.recall("fox", 4, &surfaced).len(), 1);
    std::fs::remove_dir_all(&root).ok();
}

/// When the result cap (5) entries are all surfaced, a fresh rank-6+
/// candidate must still get a chance on the next recall. The surfaced
/// filter runs BEFORE rank+truncate, so the 6th entry is not truncated
/// off by the result cap.
#[test]
fn test_recall_fresh_after_surfaced() {
    let root = temp_root();
    let p = MarkdownMemoryProvider::new(root.clone());
    for i in 0..6 {
        p.add(entry(
            &format!("fox{i}"),
            "fox body here",
            MemorySource::Project,
        ))
        .unwrap();
    }
    let mut surfaced = HashSet::new();
    // First recall surfaces up to 5 (the result cap).
    let first = p.recall("fox", 1000, &surfaced);
    assert_eq!(first.len(), 5, "first recall returns top 5");
    surfaced.extend(first.iter().map(|e| e.key.clone()));
    // Second recall must return the 6th fresh entry, not empty.
    let second = p.recall("fox", 1000, &surfaced);
    assert_eq!(
        second.len(),
        1,
        "fresh entry beyond cap must surface after top-5 are all surfaced"
    );
    surfaced.extend(second.iter().map(|e| e.key.clone()));
    // Third recall: all surfaced, returns empty.
    let third = p.recall("fox", 1000, &surfaced);
    assert!(third.is_empty(), "all surfaced must return empty");
    std::fs::remove_dir_all(&root).ok();
}

/// The index byte cap (25 KB) must cut on a UTF-8 char boundary, not a
/// mid-codepoint byte. CJK content (3 bytes/char) makes a 25000-byte cut
/// land mid-codepoint, and String::truncate panics there. This fills the
/// index past the cap with multi-byte content and asserts no panic.
#[test]
fn test_index_cap_char_boundary() {
    let root = temp_root();
    let p = MarkdownMemoryProvider::new(root.clone());
    // Each entry's index line is ~"[\u{2026}] content" with a 3-byte CJK body,
    // so the index crosses 25 KB on a non-char boundary unless the cap cut
    // walks back to a char boundary.
    let big = "\u{4e2d}".repeat(4000); // 4000 CJK chars = 12000 bytes
    for i in 0..4 {
        p.add(MemoryEntry {
            key: format!("k{i}"),
            content: big.clone(),
            tokens: 0,
            source: MemorySource::Project,
            description: String::new(),
            mtime_secs: 0,
            origin: MemoryOrigin::Unknown,
        })
        .unwrap();
    }
    // No panic, and the index file is at or under the cap.
    let index = fs::read_to_string(root.join("MEMORY.md")).unwrap();
    assert!(
        index.len() <= INDEX_BYTE_CAP + 200,
        "index {} over cap {}",
        index.len(),
        INDEX_BYTE_CAP
    );
    std::fs::remove_dir_all(&root).ok();
}

/// When the index exceeds the line cap (200 lines), it must be truncated
/// to the cap and a WARNING tail-note appended. Truncation order: line
/// cap first, then byte cap, then warning.
#[test]
fn test_index_line_cap_warning() {
    let root = temp_root();
    // A small line cap keeps the test fast: each write flushes to disk, so
    // writing hundreds of entries to exceed the production cap of 200 would
    // dominate the suite. The cap logic is the same regardless of the value.
    let p = MarkdownMemoryProvider::with_max_lines(vec![root.clone()], 5);
    // Each entry adds one short line to the index. After the cap fires a
    // WARNING is appended.
    for i in 0..(5 + 3) {
        p.add(entry(&format!("k{i}"), "short body", MemorySource::Project))
            .unwrap();
    }
    let index = fs::read_to_string(root.join(INDEX_FILE)).unwrap();
    assert!(
        index.contains("WARNING"),
        "index over line cap must carry a WARNING tail-note"
    );
    assert!(
        index.lines().count() <= 5 + 2,
        "index line count {} must be at most {} + 2 (WARNING line)",
        index.lines().count(),
        5
    );
    std::fs::remove_dir_all(&root).ok();
}

/// When append_index_pointer fails after the topic file has landed, the
/// rollback must remove the topic file. We simulate the index write
/// failure by making the index path a directory (rename-over-directory
/// fails on Unix).
#[test]
fn test_rollback_removes_topic() {
    let root = temp_root();
    let p = MarkdownMemoryProvider::new(root.clone());
    // Make the index path a directory so append_index_pointer's write
    // (temp + rename) fails: rename over a directory is rejected.
    fs::create_dir(root.join(INDEX_FILE)).unwrap();
    let res = p.add(entry("alpha", "alpha fox body", MemorySource::User));
    assert!(
        matches!(res, Err(MemoryError::AtomicityFailed(_))),
        "must return AtomicityFailed when index pointer fails"
    );
    assert!(
        !root.join("alpha.md").exists(),
        "rollback must remove the topic file"
    );
    std::fs::remove_dir_all(&root).ok();
}

/// Recall over a multi-root provider merges every scope: an entry seeded
/// into each of the user, project, and auto roots is returned together.
#[test]
fn test_recall_merges_scopes() {
    let user_dir = temp_root();
    let project_dir = temp_root();
    let auto_dir = temp_root();
    // Seed each scope via a single-root provider (add writes to its root).
    MarkdownMemoryProvider::new(user_dir.clone())
        .add(entry("user-fact", "alpha fox user", MemorySource::User))
        .unwrap();
    MarkdownMemoryProvider::new(project_dir.clone())
        .add(entry(
            "project-fact",
            "bravo fox project",
            MemorySource::Project,
        ))
        .unwrap();
    MarkdownMemoryProvider::new(auto_dir.clone())
        .add(entry(
            "auto-fact",
            "charlie fox auto",
            MemorySource::Project,
        ))
        .unwrap();
    let p = MarkdownMemoryProvider::new_multi(vec![
        user_dir.clone(),
        project_dir.clone(),
        auto_dir.clone(),
    ]);
    let out = p.recall("fox", 1000, &HashSet::new());
    assert_eq!(out.len(), 3, "recall must merge all three scopes");
    let keys: Vec<&str> = out.iter().map(|e| e.key.as_str()).collect();
    assert!(keys.contains(&"user-fact"));
    assert!(keys.contains(&"project-fact"));
    assert!(keys.contains(&"auto-fact"));
    for d in [user_dir, project_dir, auto_dir] {
        std::fs::remove_dir_all(&d).ok();
    }
}

/// The same key in two scopes dedups to one entry on recall (newest mtime
/// wins) so the merged result never returns duplicates.
#[test]
fn test_recall_dedups_across_scopes() {
    let user_dir = temp_root();
    let auto_dir = temp_root();
    MarkdownMemoryProvider::new(user_dir.clone())
        .add(entry("shared", "fox user copy", MemorySource::User))
        .unwrap();
    MarkdownMemoryProvider::new(auto_dir.clone())
        .add(entry("shared", "fox auto copy", MemorySource::Project))
        .unwrap();
    let p = MarkdownMemoryProvider::new_multi(vec![user_dir.clone(), auto_dir.clone()]);
    let out = p.recall("fox", 1000, &HashSet::new());
    assert_eq!(out.len(), 1, "same key across scopes must dedup to one");
    for d in [user_dir, auto_dir] {
        std::fs::remove_dir_all(&d).ok();
    }
}

/// new_multi drops duplicate root paths (first wins) so a degenerate config
/// where two roots resolve to the same directory scans it once, not twice.
/// Covers both branches of the dedup filter (insert true on first sight,
/// false on the duplicate).
#[test]
fn test_multi_dedups_dup_roots() {
    let user_dir = temp_root();
    let auto_dir = temp_root();
    MarkdownMemoryProvider::new(user_dir.clone())
        .add(entry("shared", "fox user copy", MemorySource::User))
        .unwrap();
    // user_dir passed twice + auto_dir once — the duplicate must drop without
    // panic and without double-scanning (which would only re-find the same
    // key, deduped by the merge HashMap either way).
    let p = MarkdownMemoryProvider::new_multi(vec![
        user_dir.clone(),
        user_dir.clone(),
        auto_dir.clone(),
    ]);
    let out = p.recall("fox", 1000, &HashSet::new());
    assert_eq!(out.len(), 1, "deduped roots still recall the one entry");
    for d in [user_dir, auto_dir] {
        std::fs::remove_dir_all(&d).ok();
    }
}

/// add on a multi-root provider lands in the last (auto) scope only, so
/// writes do not pollute the user or project scopes.
#[test]
fn test_write_lands_in_auto() {
    let user_dir = temp_root();
    let project_dir = temp_root();
    let auto_dir = temp_root();
    let p = MarkdownMemoryProvider::new_multi(vec![
        user_dir.clone(),
        project_dir.clone(),
        auto_dir.clone(),
    ]);
    p.add(entry("new-fact", "delta fox", MemorySource::Project))
        .unwrap();
    assert!(
        auto_dir.join("new-fact.md").exists(),
        "write must land in the last (auto) scope"
    );
    assert!(
        !user_dir.join("new-fact.md").exists(),
        "write must not land in the user scope"
    );
    assert!(
        !project_dir.join("new-fact.md").exists(),
        "write must not land in the project scope"
    );
    for d in [user_dir, project_dir, auto_dir] {
        std::fs::remove_dir_all(&d).ok();
    }
}

/// rebuild_if_stale self-heals: a topic file written externally (bypassing
/// add, so the index never referenced it) makes the index stale. The check
/// detects the topic is newer than the index and regenerates it, so recall
/// sees the externally-added entry without a manual rebuild.
#[test]
fn test_rebuild_heals_external_edit() {
    let root = temp_root();
    let p = MarkdownMemoryProvider::new(root.clone());
    p.add(entry("alpha", "alpha fox", MemorySource::Project))
        .unwrap();
    // Externally write a second topic, bypassing add (no index update).
    let ext = "---\nname: bravo\ndescription: bravo fox\nsource: project\n---\nbravo fox body\n";
    fs::write(root.join("bravo.md"), ext).unwrap();
    p.rebuild_if_stale().unwrap();
    let idx = fs::read_to_string(root.join(INDEX_FILE)).unwrap();
    assert!(
        idx.contains("bravo"),
        "rebuild_if_stale must pick up the externally-added topic"
    );
    let out = p.recall("bravo", 100, &HashSet::new());
    assert!(
        out.iter().any(|e| e.key == "bravo"),
        "recall must surface the healed entry"
    );
    std::fs::remove_dir_all(&root).ok();
}

/// An entry whose token count exactly fits the budget must be included;
/// one token over must break the walk (not skip ahead).
#[test]
fn test_budget_boundary_exact_fit() {
    let root = temp_root();
    let p = MarkdownMemoryProvider::new(root.clone());
    // "alpha fox" = 9 chars -> ceil(9/4) = 3 tokens.
    p.add(entry("a", "alpha fox", MemorySource::Project))
        .unwrap();
    // Budget exactly 3: the entry fits.
    let out = p.recall("fox", 3, &HashSet::new());
    assert_eq!(
        out.len(),
        1,
        "entry exactly fitting budget must be included"
    );
    // Budget 2: entry (3 tokens) does not fit -> break, empty result.
    let out = p.recall("fox", 2, &HashSet::new());
    assert!(
        out.is_empty(),
        "entry exceeding budget must break the walk, not skip ahead"
    );
    std::fs::remove_dir_all(&root).ok();
}

/// list_memories returns the frontmatter-only summary for every topic — key,
/// description, source — without reading any body content. Pins the listing
/// path the /memory command uses.
#[test]
fn test_list_memories_frontmatter() {
    let root = temp_root();
    let p = MarkdownMemoryProvider::new(root.clone());
    p.add(entry(
        "build-gate",
        "make check must stay green",
        MemorySource::Project,
    ))
    .unwrap();
    p.add(entry("rust-style", "Prefer let chains", MemorySource::User))
        .unwrap();
    let list = p.list_memories();
    assert_eq!(list.len(), 2);
    let keys: Vec<&str> = list.iter().map(|s| s.key.as_str()).collect();
    assert!(keys.contains(&"build-gate"));
    assert!(keys.contains(&"rust-style"));
    for s in &list {
        assert!(!s.description.is_empty(), "description hook present");
    }
    std::fs::remove_dir_all(&root).ok();
}

/// A corrupt topic file (unparseable frontmatter) is included in
/// list_memories with a sentinel description instead of being silently
/// dropped. The user sees it in the /memory pane so they know to fix it.
#[test]
fn test_list_includes_corrupt_file() {
    let root = temp_root();
    let p = MarkdownMemoryProvider::new(root.clone());
    p.add(entry("good-key", "a valid fact", MemorySource::Project))
        .unwrap();
    // Write a corrupt topic file (no frontmatter, garbage content).
    std::fs::write(root.join("broken.md"), "this is not valid frontmatter\n").unwrap();
    let list = p.list_memories();
    assert_eq!(list.len(), 2, "corrupt file included in list");
    let broken = list
        .iter()
        .find(|s| s.key == "broken")
        .expect("corrupt file present in list");
    assert!(
        broken.description.contains("[corrupt"),
        "corrupt file marked with sentinel: {}",
        broken.description
    );
    std::fs::remove_dir_all(&root).ok();
}

/// show_memory returns the full body (key, content, source) for an existing
/// key, None for an absent key, and None for a traversal key (sanitize_key).
#[test]
fn test_show_memory_body() {
    let root = temp_root();
    let p = MarkdownMemoryProvider::new(root.clone());
    p.add(entry(
        "build-gate",
        "make check must stay green",
        MemorySource::Project,
    ))
    .unwrap();
    let entry = p.show_memory("build-gate").expect("entry found");
    assert_eq!(entry.key, "build-gate");
    assert!(entry.content.contains("make check must stay green"));
    assert_eq!(entry.source, MemorySource::Project);
    assert!(p.show_memory("no-such-key").is_none(), "absent key → None");
    assert!(p.show_memory("../escape").is_none(), "traversal key → None");
    std::fs::remove_dir_all(&root).ok();
}

/// memory_root returns the write root path (the auto scope = last root), so
/// the consolidation dream can locate the directory + place the lock.
#[test]
fn test_memory_root_is_write() {
    let root = temp_root();
    let p = MarkdownMemoryProvider::new(root.clone());
    let got = p.memory_root();
    assert!(
        got.ends_with("memory") || got == root.to_string_lossy(),
        "memory_root is the write root: {got}"
    );
    assert!(!got.is_empty(), "non-empty for a filesystem provider");
    std::fs::remove_dir_all(&root).ok();
}

/// record_recall_hits increments recall_hits + last_access_ts and persists
/// to the .stats.json sidecar; read_recall_stats reads it back. A second
/// hit on the same key accumulates (2, not 1).
#[test]
fn test_stats_round_trip_accumulates() {
    let root = temp_root();
    let p = MarkdownMemoryProvider::new(root.clone());
    p.record_recall_hits(&["alpha".to_string(), "bravo".to_string()]);
    p.record_recall_hits(&["alpha".to_string()]);
    let stats = p.read_recall_stats();
    let by_key: std::collections::HashMap<&str, &MemoryRecallStats> =
        stats.iter().map(|s| (s.key.as_str(), s)).collect();
    let alpha = by_key["alpha"];
    assert_eq!(alpha.recall_hits, 2, "alpha hit twice → 2");
    assert!(alpha.last_access_ts > 0, "last_access stamped");
    assert_eq!(by_key["bravo"].recall_hits, 1, "bravo hit once");
    assert_eq!(
        alpha.gate_violations, 0,
        "gate_violations zero until the PreToolUse gate feeds it"
    );
    std::fs::remove_dir_all(&root).ok();
}

/// The stats sidecar is advisory: a missing sidecar yields empty stats
/// (cold restart), and a corrupt sidecar yields empty too — never a panic.
/// This is the no-self-heal/no-invariant contract.
#[test]
fn test_stats_missing_corrupt() {
    let root = temp_root();
    let p = MarkdownMemoryProvider::new(root.clone());
    assert!(
        p.read_recall_stats().is_empty(),
        "missing sidecar → empty stats (cold restart)"
    );
    std::fs::write(root.join(".stats.json"), "not valid json {{{").unwrap();
    assert!(
        p.read_recall_stats().is_empty(),
        "corrupt sidecar → empty stats, no panic"
    );
    std::fs::remove_dir_all(&root).ok();
}

/// The sidecar persists across provider instances (cross-process
/// advisory), so a fresh process reads the prior counts rather than zero.
#[test]
fn test_stats_persist_across_instances() {
    let root = temp_root();
    let p1 = MarkdownMemoryProvider::new(root.clone());
    p1.record_recall_hits(&["persisted".to_string()]);
    let p2 = MarkdownMemoryProvider::new(root.clone());
    let stats = p2.read_recall_stats();
    assert!(
        stats
            .iter()
            .any(|s| s.key == "persisted" && s.recall_hits == 1),
        "a fresh provider instance reads the persisted sidecar"
    );
    std::fs::remove_dir_all(&root).ok();
}
