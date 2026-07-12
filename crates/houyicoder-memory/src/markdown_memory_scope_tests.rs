//! Scope-dimension tests for the markdown memory provider. Extracted from
//! the main test module so that file stays under the file-size gate. Covers
//! the physical storage scope (user / project / auto) the provider exposes
//! on each MemorySummary so the /memory pane can filter by scope — the
//! dimension orthogonal to the provenance source.

use super::*;
use houyicoder_context::MemorySource;
use std::path::PathBuf;

fn temp_root() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "markdown_memory_scope_test_{seq}_{}",
        std::process::id(),
    ));
    std::fs::create_dir_all(&dir).expect("create temp root");
    dir
}

fn entry(key: &str, content: &str, source: MemorySource) -> MemoryEntry {
    MemoryEntry::new(key, content, source)
}

/// list_memories tags each summary with the storage scope of the root it
/// lives in (user / project / auto), so the /memory pane can filter by
/// scope — the physical dimension orthogonal to the provenance source. A
/// topic written to roots[0] is User, roots[1] is Project, roots[2] is Auto
/// (positional, by the documented new_multi order).
#[test]
fn test_list_scope_per_root() {
    use houyicoder_context::MemoryScope;
    let user_dir = temp_root();
    let project_dir = temp_root();
    let auto_dir = temp_root();
    let put = |dir, key, src| {
        MarkdownMemoryProvider::new(dir)
            .add(entry(key, "fact", src))
            .unwrap();
    };
    put(user_dir.clone(), "user-fact", MemorySource::User);
    put(project_dir.clone(), "project-fact", MemorySource::Project);
    put(auto_dir.clone(), "auto-fact", MemorySource::Feedback);
    let p = MarkdownMemoryProvider::new_multi(vec![
        user_dir.clone(),
        project_dir.clone(),
        auto_dir.clone(),
    ]);
    let want = [
        ("user-fact", MemoryScope::User),
        ("project-fact", MemoryScope::Project),
        ("auto-fact", MemoryScope::Auto),
    ];
    let list = p.list_memories();
    assert_eq!(list.len(), want.len());
    for s in &list {
        let expected = want
            .iter()
            .find(|(k, _)| *k == s.key)
            .map(|(_, sc)| *sc)
            .unwrap_or_else(|| panic!("unexpected key {}", s.key));
        assert_eq!(s.scope, expected, "scope matches root for {}", s.key);
    }
    for d in [user_dir, project_dir, auto_dir] {
        std::fs::remove_dir_all(&d).ok();
    }
}

/// count_new_since counts topic files newer than the given timestamp. The
/// dream gate uses this to decide whether new material landed since the
/// last dream; pin the markdown impl so the gate's input is trustworthy.
#[test]
fn test_count_new_since_topics() {
    let root = temp_root();
    let p = MarkdownMemoryProvider::new(root.clone());
    p.add(entry("alpha", "a fact", MemorySource::Project))
        .unwrap();
    p.add(entry("beta", "b fact", MemorySource::User)).unwrap();
    p.add(entry("gamma", "c fact", MemorySource::Reference))
        .unwrap();
    // All three seeded now have mtime past the epoch.
    assert_eq!(p.count_new_since(0), 3, "three new topics since epoch");
    // A future timestamp sees none.
    let future = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    assert_eq!(
        p.count_new_since(future),
        0,
        "no topics newer than a future timestamp"
    );
    std::fs::remove_dir_all(&root).ok();
}
