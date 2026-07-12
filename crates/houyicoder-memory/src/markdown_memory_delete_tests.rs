//! Delete-path tests for the markdown memory provider: auto-scope delete
//! (the dream's prune), scoped delete routing (the /memory pane forget), and
//! stats-sidecar pruning on delete. Split out of the main test module so that
//! file stays under the file-size gate. Like the scope_tests split: each
//! split file carries its own temp_root + entry helpers.

use super::*;
use houyicoder_context::MemorySource;
use std::path::PathBuf;

fn temp_root() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "markdown_memory_delete_test_{seq}_{}",
        std::process::id(),
    ));
    std::fs::create_dir_all(&dir).expect("create temp root");
    dir
}

fn entry(key: &str, content: &str, source: MemorySource) -> MemoryEntry {
    MemoryEntry::new(key, content, source)
}

/// delete_memory removes the topic file and regenerates the index so the
/// pointer disappears. Recall no longer returns the deleted key. Deleting an
/// absent key returns NotFound; deleting a traversal key is rejected.
#[test]
fn test_delete_removes_topic_index() {
    let root = temp_root();
    let p = MarkdownMemoryProvider::new(root.clone());
    p.add(entry("keep-me", "a fact to keep", MemorySource::Project))
        .unwrap();
    p.add(entry(
        "prune-me",
        "a stale fact to prune",
        MemorySource::Project,
    ))
    .unwrap();
    assert_eq!(
        p.list_memories().len(),
        2,
        "both entries listed before delete"
    );
    p.delete_memory("prune-me").expect("delete succeeds");
    assert!(!root.join("prune-me.md").exists(), "topic file removed");
    assert!(
        root.join("keep-me.md").exists(),
        "unrelated topic untouched"
    );
    let keys: Vec<String> = p.list_memories().into_iter().map(|s| s.key).collect();
    assert!(
        !keys.contains(&"prune-me".to_string()),
        "deleted key no longer listed (index regenerated)"
    );
    assert!(
        keys.contains(&"keep-me".to_string()),
        "unrelated key still listed"
    );
    assert!(
        p.show_memory("prune-me").is_none(),
        "deleted key no longer readable"
    );
    assert!(
        matches!(p.delete_memory("prune-me"), Err(MemoryError::NotFound)),
        "deleting an absent key returns NotFound"
    );
    assert!(
        matches!(
            p.delete_memory("../escape"),
            Err(MemoryError::InvalidPath(_))
        ),
        "traversal key rejected before any fs op"
    );
    std::fs::remove_dir_all(&root).ok();
}

/// delete_memory_in_scope routes the delete to the matching root: forgetting
/// a project-scope row deletes the project-root file, and an auto-scope
/// delete of a key that lives only in project returns NotFound (no silent
/// auto fallback that would mask the missing explicit original).
#[test]
fn test_delete_routes_by_scope() {
    use houyicoder_context::MemoryScope;
    let user_dir = temp_root();
    let project_dir = temp_root();
    let auto_dir = temp_root();
    let p = MarkdownMemoryProvider::new_multi(vec![
        user_dir.clone(),
        project_dir.clone(),
        auto_dir.clone(),
    ]);
    p.add_in_scope(
        entry("proj-fact", "a project-scoped fact", MemorySource::Project),
        MemoryScope::Project,
    )
    .unwrap();
    assert!(
        project_dir.join("proj-fact.md").exists(),
        "seeded in project root"
    );
    // Forgetting with the project scope deletes the project-root file.
    p.delete_memory_in_scope("proj-fact", MemoryScope::Project)
        .expect("scoped delete succeeds");
    assert!(
        !project_dir.join("proj-fact.md").exists(),
        "project root file removed"
    );
    // Forgetting the same key from the auto scope returns NotFound: the key
    // is not in the auto root. A silent auto fallback would have masked the
    // missing explicit original — the bug this routing fixes.
    assert!(
        matches!(
            p.delete_memory_in_scope("proj-fact", MemoryScope::Auto),
            Err(MemoryError::NotFound)
        ),
        "auto-scope delete of a project-only key must NotFound, not silently no-op"
    );
    for d in [user_dir, project_dir, auto_dir] {
        std::fs::remove_dir_all(&d).ok();
    }
}

/// delete_memory prunes the stats sidecar entry so it does not grow
/// monotonically with deleted memories (the dream prunes stale entries;
/// their stats rows must not linger + slow the hot-path record_recall_hits).
#[test]
fn test_delete_prunes_stats_sidecar() {
    let root = temp_root();
    let p = MarkdownMemoryProvider::new(root.clone());
    p.add(entry("doomed", "a fact to delete", MemorySource::Project))
        .unwrap();
    p.record_recall_hits(&["doomed".to_string()]);
    assert!(
        p.read_recall_stats().iter().any(|s| s.key == "doomed"),
        "stats recorded before delete"
    );
    p.delete_memory("doomed").unwrap();
    assert!(
        !p.read_recall_stats().iter().any(|s| s.key == "doomed"),
        "stats entry pruned on delete (no monotonic growth)"
    );
    std::fs::remove_dir_all(&root).ok();
}
