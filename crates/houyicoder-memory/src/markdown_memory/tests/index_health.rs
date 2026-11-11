//! Index-health tests: the dirty marker a writer leaves when its index update
//! does not land, and the staleness checks that make the next session rebuild
//! from the topic files.

use super::super::index_health::INDEX_DIRTY;
use super::super::index_health::fail_marker_after;
use super::super::io::INDEX_TRUNCATION_WARNING;
use super::super::io::fail_next_index_write;
use super::super::*;
use houyicoder_context::MemoryEntry;
use houyicoder_context::MemoryError;
use houyicoder_context::MemoryScope;
use houyicoder_context::MemorySource;
use std::env::temp_dir;
use std::fs::File;
use std::fs::create_dir_all;
use std::fs::metadata;
use std::fs::read_to_string;
use std::fs::remove_dir_all;
use std::fs::remove_file;
use std::fs::write;
use std::path::Path;
use std::path::PathBuf;
use std::process::id;
use std::sync::atomic::{AtomicU64, Ordering};

fn temp_root(tag: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = temp_dir().join(format!("markdown-index-{tag}-{seq}-{}", id()));
    create_dir_all(&dir).expect("create temp root");
    dir
}

fn entry(key: &str, content: &str) -> MemoryEntry {
    MemoryEntry::new(key, content, MemorySource::Project)
}

fn index_text(root: &Path) -> String {
    read_to_string(root.join("MEMORY.md")).unwrap_or_default()
}

/// A three-root provider laid out like the composition root: user, project,
/// and the automatic write root.
struct ThreeRoots {
    workspace: PathBuf,
    user: PathBuf,
    project: PathBuf,
    auto: PathBuf,
    provider: MarkdownMemoryProvider,
}

impl ThreeRoots {
    fn new(tag: &str) -> Self {
        let workspace = temp_root(tag);
        let user = workspace.join("user");
        let project = workspace.join("project");
        let auto = workspace.join("auto");
        for dir in [&user, &project, &auto] {
            create_dir_all(dir).expect("create a scope root");
        }
        let provider =
            MarkdownMemoryProvider::new_multi(vec![user.clone(), project.clone(), auto.clone()]);
        Self {
            workspace,
            user,
            project,
            auto,
            provider,
        }
    }

    fn reopen(&self) -> MarkdownMemoryProvider {
        MarkdownMemoryProvider::new_multi(vec![
            self.user.clone(),
            self.project.clone(),
            self.auto.clone(),
        ])
    }
}

impl Drop for ThreeRoots {
    fn drop(&mut self) {
        drop(remove_dir_all(&self.workspace));
    }
}

/// A marker left by a writer that died before publishing the index makes the
/// next session rebuild, even though every topic is older than the index.
#[test]
fn test_marker_triggers_rebuild() {
    let root = temp_root("marker");
    let provider = MarkdownMemoryProvider::new(root.clone());
    provider
        .add(entry("alpha", "alpha body"))
        .expect("seed a topic");
    assert!(
        !root.join(INDEX_DIRTY).exists(),
        "a completed add clears the marker it raised"
    );
    write(root.join(INDEX_DIRTY), b"").expect("leave a marker");

    provider.rebuild_if_stale().expect("the marked index heals");

    assert!(
        !root.join(INDEX_DIRTY).exists(),
        "a completed rebuild clears the marker"
    );
    assert!(
        index_text(&root).contains("- alpha ["),
        "the rebuilt index still names the topic"
    );
    remove_dir_all(&root).ok();
}

/// Deleting a topic behind the provider's back leaves an index row naming a
/// file that is gone. No modification time betrays it, so the rebuild has to
/// read the published rows themselves.
#[test]
fn test_dangling_pointer_heals() {
    let root = temp_root("dangling");
    let provider = MarkdownMemoryProvider::new(root.clone());
    for (key, body) in [
        ("keep-me", "kept body"),
        ("two words", "spaced key body"),
        ("gone", "removed body"),
    ] {
        provider.add(entry(key, body)).expect("seed a topic");
    }
    assert!(
        index_text(&root).contains("- gone ["),
        "the index names the topic before the deletion"
    );
    remove_file(root.join("gone.md")).expect("delete the topic directly");

    provider.rebuild_if_stale().expect("the dangling row heals");

    let text = index_text(&root);
    assert!(
        !text.contains("- gone ["),
        "the rebuilt index drops the row naming a missing topic:\n{text}"
    );
    assert!(text.contains("- keep-me ["), "unrelated rows survive");
    assert!(
        text.contains("- two words ["),
        "a key containing a space is still one row"
    );
    remove_dir_all(&root).ok();
}

/// An add whose index append fails keeps the topic and the old index, and
/// leaves the marker so the next session adds the missing row.
#[test]
fn test_add_failure_marks_index() {
    let root = temp_root("add-failure");
    let provider = MarkdownMemoryProvider::new(root.clone());
    provider
        .add(entry("keep-me", "kept body"))
        .expect("seed the kept topic");
    fail_next_index_write();

    provider
        .add(entry("new", "new body"))
        .expect("the write lands");

    assert!(root.join("new.md").exists(), "the topic is on disk");
    assert!(
        root.join(INDEX_DIRTY).exists(),
        "an add whose index append failed must leave the marker"
    );
    assert!(
        !index_text(&root).contains("- new ["),
        "the old index does not name the new topic"
    );

    provider.rebuild_if_stale().expect("the next session heals");

    assert!(
        !root.join(INDEX_DIRTY).exists(),
        "the heal clears the marker"
    );
    assert!(
        index_text(&root).contains("- new ["),
        "the healed index names the topic that was already on disk"
    );
    remove_dir_all(&root).ok();
}

/// A marker that cannot be created stops the write before the topic changes.
/// The marker path is a directory here, so the topic write itself would have
/// succeeded; the add and the delete both refuse instead.
#[test]
fn test_marker_failure_blocks_write() {
    let root = temp_root("marker-blocked");
    let provider = MarkdownMemoryProvider::new(root.clone());
    provider
        .add(entry("alpha", "alpha body"))
        .expect("seed the topic");
    create_dir_all(root.join(INDEX_DIRTY)).expect("block the marker path");

    let added = provider.add(entry("beta", "beta body"));
    assert!(matches!(added, Err(MemoryError::Io)));
    assert!(
        !root.join("beta.md").exists(),
        "the add refuses before writing the topic"
    );

    let deleted = provider.delete_memory("alpha");
    assert!(matches!(deleted, Err(MemoryError::Io)));
    assert!(
        root.join("alpha.md").exists(),
        "the delete refuses before removing the topic"
    );
    remove_dir_all(&root).ok();
}

/// A marker left by an earlier failed update survives a later successful add.
/// The add appends only its own row, so clearing the marker would let the
/// earlier missing row stay invisible to the next session's checks.
#[test]
fn test_stale_marker_survives_add() {
    let root = temp_root("stale-marker");
    let provider = MarkdownMemoryProvider::new(root.clone());
    provider
        .add(entry("first", "first body"))
        .expect("seed the first topic");
    write(root.join(INDEX_DIRTY), b"").expect("leave a marker");

    provider
        .add(entry("second", "second body"))
        .expect("the later add lands");

    assert!(
        root.join(INDEX_DIRTY).exists(),
        "the later add must not clear a marker it did not create"
    );

    provider.rebuild_if_stale().expect("the next session heals");

    assert!(
        !root.join(INDEX_DIRTY).exists(),
        "the heal clears the marker"
    );
    remove_dir_all(&root).ok();
}

/// A delete whose index rewrite fails keeps the old index readable and leaves
/// the marker behind, so the next session repairs the index from the topics
/// instead of trusting a row naming the deleted key.
#[test]
fn test_delete_failure_leaves_marker() {
    let root = temp_root("delete-failure");
    let provider = MarkdownMemoryProvider::new(root.clone());
    provider
        .add(entry("keep-me", "kept body"))
        .expect("seed the kept topic");
    provider
        .add(entry("prune-me", "pruned body"))
        .expect("seed the pruned topic");
    fail_next_index_write();

    provider
        .delete_memory("prune-me")
        .expect("the delete lands");

    assert!(!root.join("prune-me.md").exists(), "the topic is gone");
    assert!(
        root.join(INDEX_DIRTY).exists(),
        "a delete whose index publish failed must leave the marker"
    );
    assert!(
        index_text(&root).contains("- prune-me ["),
        "the published index is left as it was"
    );

    provider.rebuild_if_stale().expect("the next session heals");

    assert!(
        !root.join(INDEX_DIRTY).exists(),
        "the heal clears the marker"
    );
    assert!(
        !index_text(&root).contains("- prune-me ["),
        "the healed index no longer names the deleted topic"
    );
    assert!(
        index_text(&root).contains("- keep-me ["),
        "the remaining topic keeps its row"
    );
    remove_dir_all(&root).ok();
}

/// Demoting with a failed index rebuild moves the topic but publishes neither
/// index. No topic is newer than either index afterwards, so only the markers
/// can tell the next session to repair both roots.
#[test]
fn test_demote_failure_marks_both() {
    let fx = ThreeRoots::new("demote");
    fx.provider
        .add_in_scope(entry("topic", "topic body"), MemoryScope::Project)
        .expect("seed the project topic");
    fx.provider
        .add(entry("other", "other body"))
        .expect("seed an automatic topic");
    assert!(
        index_text(&fx.project).contains("- topic ["),
        "the project index names the topic before the move"
    );
    fail_next_index_write();

    fx.provider.demote_memory("topic").expect("the move lands");

    assert!(fx.auto.join("topic.md").exists(), "the topic moved");
    assert!(
        !fx.project.join("topic.md").exists(),
        "the project copy is gone"
    );
    assert!(
        fx.project.join(INDEX_DIRTY).exists(),
        "the project root is marked for repair"
    );
    assert!(
        fx.auto.join(INDEX_DIRTY).exists(),
        "the automatic root is marked for repair"
    );
    assert!(
        index_text(&fx.project).contains("- topic ["),
        "the project index keeps its old row"
    );

    fx.reopen()
        .rebuild_if_stale()
        .expect("the next session heals");

    assert!(
        !index_text(&fx.project).contains("- topic ["),
        "the healed project index no longer names the moved topic"
    );
    assert!(
        index_text(&fx.auto).contains("- topic ["),
        "the healed automatic index gains the moved topic"
    );
    assert!(!fx.project.join(INDEX_DIRTY).exists());
    assert!(!fx.auto.join(INDEX_DIRTY).exists());
}

/// Promotion has the same two-root exposure as demotion: the marker goes down
/// on both roots before the topic moves, so a failed publish is repaired on
/// the next start.
#[test]
fn test_promote_failure_marks_both() {
    let fx = ThreeRoots::new("promote");
    fx.provider
        .add(entry("topic", "topic body"))
        .expect("seed the automatic topic");
    assert!(
        index_text(&fx.auto).contains("- topic ["),
        "the automatic index names the topic before the move"
    );
    fail_next_index_write();

    fx.provider.promote_memory("topic").expect("the move lands");

    assert!(fx.project.join("topic.md").exists(), "the topic moved");
    assert!(
        !fx.auto.join("topic.md").exists(),
        "the automatic copy is gone"
    );
    assert!(
        fx.project.join(INDEX_DIRTY).exists(),
        "the project root is marked for repair"
    );
    assert!(
        fx.auto.join(INDEX_DIRTY).exists(),
        "the automatic root is marked for repair"
    );

    fx.reopen()
        .rebuild_if_stale()
        .expect("the next session heals");

    assert!(
        index_text(&fx.project).contains("- topic ["),
        "the healed project index names the moved topic"
    );
    assert!(
        !index_text(&fx.auto).contains("- topic ["),
        "the healed automatic index drops the moved topic"
    );
    assert!(!fx.project.join(INDEX_DIRTY).exists());
    assert!(!fx.auto.join(INDEX_DIRTY).exists());
}

/// The index row is addressed by the topic's file stem, the key every other
/// path resolves, even when the topic's own name field differs. A row
/// addressed any other way reads back as a dangling pointer on every start.
#[test]
fn test_index_row_uses_stem() {
    let root = temp_root("stem-row");
    let provider = MarkdownMemoryProvider::new(root.clone());
    let external = "---\nname: Display Name\ndescription: described\nsource: project\n---\nbody\n";
    write(root.join("stem.md"), external).expect("seed an external topic");

    provider.rebuild_index().expect("publish the row");

    let text = index_text(&root);
    assert!(
        text.contains("- stem ["),
        "the row uses the file stem:\n{text}"
    );
    assert!(
        !text.contains("- Display Name ["),
        "the name field does not address the row:\n{text}"
    );
    write(root.join("MEMORY.md"), format!("{text}sentinel\n")).expect("mark the index");

    provider
        .rebuild_if_stale()
        .expect("a store addressed by stems is current");

    assert!(
        index_text(&root).contains("sentinel"),
        "the heal leaves a current index in place"
    );
    remove_dir_all(&root).ok();
}

/// A key holding a space and a bracket is still one row: the published rows
/// are read back as the key that was written, not as a shorter key followed
/// by description text.
#[test]
fn test_delimiter_key_pointer_heals() {
    let root = temp_root("delimiter-key");
    let provider = MarkdownMemoryProvider::new(root.clone());
    provider
        .add(entry("foo", "plain body"))
        .expect("seed the plain key");
    let legacy = "---\nname: foo [bar\ndescription: bracket body\nsource: project\n---\nbody\n";
    write(root.join("foo [bar.md"), legacy).expect("seed the legacy topic");
    provider.rebuild_index().expect("publish both rows");
    let text = index_text(&root);
    assert!(
        text.contains("- foo [bar ["),
        "the legacy key is one row:\n{text}"
    );
    remove_file(root.join("foo [bar.md")).expect("delete the legacy topic");

    provider
        .rebuild_if_stale()
        .expect("the dangling legacy row heals");

    let text = index_text(&root);
    assert!(
        !text.contains("- foo [bar ["),
        "the deleted key's row is dropped:\n{text}"
    );
    assert!(
        text.contains("- foo ["),
        "the surviving key keeps its row:\n{text}"
    );
    remove_dir_all(&root).ok();
}

/// An external copy that kept an older timestamp still appears in the index:
/// no topic is newer than the index, so only the published rows can show the
/// omission.
#[test]
fn test_equal_mtime_heals() {
    let root = temp_root("equal-mtime");
    let provider = MarkdownMemoryProvider::new(root.clone());
    provider
        .add(entry("alpha", "alpha body"))
        .expect("seed a topic");
    let index_time = metadata(root.join("MEMORY.md"))
        .expect("the index metadata")
        .modified()
        .expect("the index mtime");
    let external = "---\nname: beta\ndescription: beta body\nsource: project\n---\nbeta body\n";
    write(root.join("beta.md"), external).expect("copy a topic in");
    File::options()
        .write(true)
        .open(root.join("beta.md"))
        .expect("open the copied topic")
        .set_modified(index_time)
        .expect("keep the copy's older timestamp");
    assert!(
        !index_text(&root).contains("- beta ["),
        "the index predates the copy"
    );

    provider.rebuild_if_stale().expect("the missing row heals");

    assert!(
        index_text(&root).contains("- beta ["),
        "the healed index names the copied topic"
    );
    remove_dir_all(&root).ok();
}

/// A capped index omits topics on purpose. The completeness check must read
/// the warning and leave the index alone; otherwise every start repairs it
/// without adding a row.
#[test]
fn test_capped_index_skips_rebuild() {
    let root = temp_root("capped-index");
    let provider = MarkdownMemoryProvider::with_max_lines(vec![root.clone()], 3);
    for key in ["one", "two", "three", "four"] {
        provider.add(entry(key, "body")).expect("seed a topic");
    }
    let text = index_text(&root);
    assert!(
        text.contains("index too long"),
        "the index is capped:\n{text}"
    );
    assert!(
        !text.contains("- four ["),
        "the cap leaves later topics out:\n{text}"
    );
    write(root.join("MEMORY.md"), format!("{text}\nsentinel\n")).expect("mark the index");

    MarkdownMemoryProvider::with_max_lines(vec![root.clone()], 3)
        .rebuild_if_stale()
        .expect("a capped index is not drift");

    assert!(
        index_text(&root).contains("sentinel"),
        "a capped index is left in place"
    );
    remove_dir_all(&root).ok();
}

/// A delete raises the marker before removing the topic. When the repair that
/// follows cannot even mark the root, the marker the delete raised survives,
/// so the next session still learns that a row is stale.
#[test]
fn test_delete_marker_survives() {
    let root = temp_root("delete-marker");
    let provider = MarkdownMemoryProvider::new(root.clone());
    provider
        .add(entry("alpha", "alpha body"))
        .expect("seed the removed topic");
    provider
        .add(entry("beta", "beta body"))
        .expect("seed the kept topic");
    fail_marker_after(1);

    provider
        .delete_memory("alpha")
        .expect("the topic removal succeeds");

    assert!(!root.join("alpha.md").exists(), "the topic is gone");
    assert!(
        root.join(INDEX_DIRTY).exists(),
        "the marker the delete raised survives a failed repair"
    );
    assert!(
        index_text(&root).contains("- alpha ["),
        "the failed repair leaves the old index"
    );

    provider.rebuild_if_stale().expect("the next session heals");

    assert!(!root.join(INDEX_DIRTY).exists());
    assert!(!index_text(&root).contains("- alpha ["));
}

/// A new key cannot contain the sequence that separates a row's key from its
/// label, so the index can always read its own rows back as whole keys.
#[test]
fn test_key_separator_rejected() {
    let root = temp_root("separator-key");
    let provider = MarkdownMemoryProvider::new(root.clone());

    let added = provider.add(entry("foo [bar", "body"));
    assert!(matches!(added, Err(MemoryError::InvalidPath(_))));
    assert!(
        !root.join("foo [bar.md").exists(),
        "the refused key leaves no topic"
    );

    let scoped = provider.add_in_scope(entry("foo [bar", "body"), MemoryScope::Project);
    assert!(matches!(scoped, Err(MemoryError::InvalidPath(_))));
    remove_dir_all(&root).ok();
}

/// The completeness check reads the truncation warning as a whole line. A
/// description that repeats the sentence must not hide a topic the index
/// omits.
#[test]
fn test_warning_text_in_description() {
    let root = temp_root("warning-text");
    let provider = MarkdownMemoryProvider::new(root.clone());
    provider
        .add(entry("alpha", INDEX_TRUNCATION_WARNING))
        .expect("seed a topic whose description repeats the warning");
    let index_time = metadata(root.join("MEMORY.md"))
        .expect("the index metadata")
        .modified()
        .expect("the index mtime");
    let external = "---\nname: beta\ndescription: beta body\nsource: project\n---\nbeta body\n";
    write(root.join("beta.md"), external).expect("copy a topic in");
    File::options()
        .write(true)
        .open(root.join("beta.md"))
        .expect("open the copied topic")
        .set_modified(index_time)
        .expect("keep the copy's older timestamp");
    assert!(
        index_text(&root).contains(INDEX_TRUNCATION_WARNING),
        "the index carries the sentence inside a row"
    );

    provider
        .rebuild_if_stale()
        .expect("a description cannot hide the omitted topic");

    assert!(
        index_text(&root).contains("- beta ["),
        "the omitted topic heals"
    );
    remove_dir_all(&root).ok();
}

/// A colon in a legacy key cannot be a file name on Windows, so the scenario
/// exists only on Unix.
#[cfg(unix)]
#[test]
fn test_legacy_key_skips_rebuild() {
    let root = temp_root("legacy-ambiguous");
    let provider = MarkdownMemoryProvider::new(root.clone());
    provider
        .add(entry("alpha", "alpha body"))
        .expect("seed a topic");
    let index_time = metadata(root.join("MEMORY.md"))
        .expect("the index metadata")
        .modified()
        .expect("the index mtime");
    let legacy =
        "---\nname: notes [user]: v2\ndescription: legacy body\nsource: project\n---\nbody\n";
    write(root.join("notes [user]: v2.md"), legacy).expect("seed the legacy topic");
    File::options()
        .write(true)
        .open(root.join("notes [user]: v2.md"))
        .expect("open the legacy topic")
        .set_modified(index_time)
        .expect("keep the legacy timestamp");
    let text = index_text(&root);
    write(root.join("MEMORY.md"), format!("{text}sentinel\n")).expect("mark the index");

    provider
        .rebuild_if_stale()
        .expect("an unaddressable legacy key is not drift");

    assert!(
        index_text(&root).contains("sentinel"),
        "the index is left in place"
    );
    remove_dir_all(&root).ok();
}

/// A rebuild does not publish a row for a key no row can address, so a
/// later start cannot read that row as a different key and repair again.
#[cfg(unix)]
#[test]
fn test_rebuild_skips_legacy_key() {
    let root = temp_root("legacy-skip");
    let provider = MarkdownMemoryProvider::new(root.clone());
    provider
        .add(entry("alpha", "alpha body"))
        .expect("seed a topic");
    let legacy =
        "---\nname: notes [user]: v2\ndescription: legacy body\nsource: project\n---\nbody\n";
    write(root.join("notes [user]: v2.md"), legacy).expect("seed the legacy topic");
    write(root.join(INDEX_DIRTY), b"").expect("force a rebuild");

    provider.rebuild_if_stale().expect("the marked index heals");

    let text = index_text(&root);
    assert!(
        !text.contains("notes [user]"),
        "an unaddressable row is not published:\n{text}"
    );
    assert!(
        text.contains("- alpha ["),
        "the addressable topic keeps its row:\n{text}"
    );
    remove_dir_all(&root).ok();
}
