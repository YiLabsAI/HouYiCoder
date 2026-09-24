//! Scope-dimension tests for the markdown memory provider. Extracted from
//! the main test module so that file stays under the file-size gate. Covers
//! the physical storage scope (user / project / auto) the provider exposes
//! on each MemorySummary so the /memory pane can filter by scope — the
//! dimension orthogonal to the provenance source.

use super::*;
use houyicoder_context::MemorySource;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_root() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!(
        "markdown_memory_scope_test_{seq}_{}",
        process::id(),
    ));
    fs::create_dir_all(&dir).expect("create temp root");
    dir
}

fn entry(key: &str, content: &str, source: MemorySource) -> MemoryEntry {
    MemoryEntry::new(key, content, source)
}

/// A key stored in two roots has two bodies. The merged scan keeps only the
/// newest, so scopes_for_key is the one call that can report the second copy,
/// and a named read returns the copy the caller asked for rather than the
/// newest one.
#[test]
fn test_show_scoped_body() {
    use houyicoder_context::MemoryScope;
    let user_dir = temp_root();
    let project_dir = temp_root();
    let auto_dir = temp_root();
    MarkdownMemoryProvider::new(user_dir.clone())
        .add(entry(
            "rule-x",
            "body from the user root",
            MemorySource::User,
        ))
        .unwrap();
    MarkdownMemoryProvider::new(auto_dir.clone())
        .add(entry(
            "rule-x",
            "body from the auto root",
            MemorySource::Feedback,
        ))
        .unwrap();
    let p = MarkdownMemoryProvider::new_multi(vec![
        user_dir.clone(),
        project_dir.clone(),
        auto_dir.clone(),
    ]);

    let mut scopes = p.scopes_for_key("rule-x");
    scopes.sort_by_key(|s| s.as_label());
    assert_eq!(
        scopes,
        vec![MemoryScope::Auto, MemoryScope::User],
        "both roots holding the key are reported"
    );
    assert_eq!(
        p.list_memories().len(),
        1,
        "the merged listing shows one row, so only scopes_for_key sees the second"
    );
    assert_eq!(
        p.show_memory_in_scope("rule-x", MemoryScope::User)
            .expect("user copy")
            .content,
        "body from the user root"
    );
    assert_eq!(
        p.show_memory_in_scope("rule-x", MemoryScope::Auto)
            .expect("auto copy")
            .content,
        "body from the auto root"
    );
    assert!(
        p.show_memory_in_scope("rule-x", MemoryScope::Project)
            .is_none(),
        "a root without the key yields nothing rather than another root's copy"
    );
    for d in [user_dir, project_dir, auto_dir] {
        fs::remove_dir_all(&d).ok();
    }
}

/// A key absent from every root reports no scopes, and one present in a single
/// root reports exactly that root — so a caller can tell an absent key from an
/// ambiguous one before it reads.
#[test]
fn test_scopes_for_key_absent() {
    use houyicoder_context::MemoryScope;
    let user_dir = temp_root();
    let project_dir = temp_root();
    let auto_dir = temp_root();
    MarkdownMemoryProvider::new(user_dir.clone())
        .add(entry("only-here", "one copy", MemorySource::User))
        .unwrap();
    let p = MarkdownMemoryProvider::new_multi(vec![
        user_dir.clone(),
        project_dir.clone(),
        auto_dir.clone(),
    ]);
    assert_eq!(
        p.scopes_for_key("only-here"),
        vec![MemoryScope::User],
        "a single copy reports its own root"
    );
    assert!(
        p.scopes_for_key("nowhere").is_empty(),
        "an absent key reports no scope, not a default root"
    );
    for d in [user_dir, project_dir, auto_dir] {
        fs::remove_dir_all(&d).ok();
    }
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
        fs::remove_dir_all(&d).ok();
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
    let future = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    assert_eq!(
        p.count_new_since(future),
        0,
        "no topics newer than a future timestamp"
    );
    fs::remove_dir_all(&root).ok();
}
