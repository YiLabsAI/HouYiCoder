//! Tests for the scope-flow operations: add_in_scope, promote_memory, and
//! demote_memory. Verifies the file-op sequence the design pins (topic move
//! + carrier merge / strip + index rebuild), idempotency, and the
//!   MED-1 closure (a project-scope refresh no longer shadows the explicit
//!   entry with a competing auto copy).

use super::super::*;
use houyicoder_context::{MemoryEntry, MemoryScope, MemorySource};
use std::collections::HashMap;
use std::env::temp_dir;
use std::fs::OpenOptions;
use std::fs::create_dir_all;
use std::fs::metadata;
use std::fs::read_dir;
use std::fs::read_to_string;
use std::fs::remove_dir_all;
use std::fs::rename;
use std::process::id;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// A three-root provider rooted at a temp workspace, mirroring the
/// composition-root layout: user / project / auto roots under a workspace
/// dir. The workspace root itself holds no memory file: a promoted memory
/// lives in the project root and reaches the prompt through the index.
struct ThreeScopeFixture {
    user_root: PathBuf,
    project_root: PathBuf,
    auto_root: PathBuf,
    workspace: PathBuf,
    provider: MarkdownMemoryProvider,
}

impl ThreeScopeFixture {
    fn new() -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let workspace = temp_dir().join(format!("scope-flow-{seq}-{}", id(),));
        // Layout matches the composition root: user / project / auto roots
        // under one workspace dir, with the auto root as the write target.
        let user_root = workspace.join("user-memory");
        let project_root = workspace.join(".config").join("memory");
        let auto_root = workspace.join("auto-memory");
        create_dir_all(&user_root).expect("mkdir user");
        create_dir_all(&project_root).expect("mkdir project");
        create_dir_all(&auto_root).expect("mkdir auto");
        let provider = MarkdownMemoryProvider::new_multi(vec![
            user_root.clone(),
            project_root.clone(),
            auto_root.clone(),
        ]);
        Self {
            user_root,
            project_root,
            auto_root,
            workspace,
            provider,
        }
    }

    /// The workspace root. A promote must leave it untouched: the user's
    /// repository is not a memory store.
    fn workspace_root(&self) -> &Path {
        &self.workspace
    }

    fn entry(&self, key: &str, content: &str) -> MemoryEntry {
        MemoryEntry::new(key, content, MemorySource::Feedback).with_meta(format!("{key} hook"), 0)
    }
}

impl Drop for ThreeScopeFixture {
    fn drop(&mut self) {
        drop(remove_dir_all(&self.workspace));
    }
}

/// add_in_scope(project) lands the topic in the project root, not the auto
/// root. The MED-1 closure: a project-scope refresh no longer shadows the
/// explicit entry with a competing auto copy (the auto root stays empty).
#[test]
fn test_add_scope_writes_target() {
    let fx = ThreeScopeFixture::new();
    let entry = fx.entry("rule-no-backticks", "No backticks in comments.\nWhy: ...\n");
    fx.provider
        .add_in_scope(entry, MemoryScope::Project)
        .expect("project-scope add");
    let project_topic = fx.project_root.join("rule-no-backticks.md");
    let auto_topic = fx.auto_root.join("rule-no-backticks.md");
    assert!(project_topic.is_file(), "topic landed in project root");
    assert!(
        !auto_topic.is_file(),
        "auto root stays empty (MED-1 closure)"
    );
    // list_memories sees it under the Project scope.
    let listed: Vec<_> = fx
        .provider
        .list_memories()
        .into_iter()
        .filter(|m| m.key == "rule-no-backticks")
        .collect();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].scope, MemoryScope::Project);
}

/// add_in_scope(auto) lands the topic in the auto root, matching the default
/// add behavior. The scope parameter is honored for both values.
#[test]
fn test_add_scope_auto_default() {
    let fx = ThreeScopeFixture::new();
    let entry = fx.entry("k-auto", "rule\n");
    fx.provider
        .add_in_scope(entry.clone(), MemoryScope::Auto)
        .expect("auto-scope add");
    let auto_topic = fx.auto_root.join("k-auto.md");
    assert!(auto_topic.is_file(), "topic landed in auto root");
    // Default add also lands in auto (the last root).
    let entry2 = fx.entry("k-default", "rule\n");
    fx.provider.add(entry2).expect("default add");
    assert!(fx.auto_root.join("k-default.md").is_file());
}

/// promote_memory moves a topic from auto into project. The workspace root
/// is left untouched, and the topic is still recallable under the project
/// scope, which is what makes the promoted rule always-on.
#[test]
fn test_promote_moves_and_merges() {
    let fx = ThreeScopeFixture::new();
    // Seed an auto-scope topic.
    let entry = fx.entry(
        "rule-test-naming",
        "Test names use verb-object.\nWhy: clarity.\nHow: foo_bar_baz.\n",
    );
    fx.provider.add(entry).expect("seed auto");
    fx.provider
        .promote_memory("rule-test-naming")
        .expect("promote");
    // Topic moved auto -> project.
    assert!(
        !fx.auto_root.join("rule-test-naming.md").is_file(),
        "auto topic gone"
    );
    assert!(
        fx.project_root.join("rule-test-naming.md").is_file(),
        "project topic present"
    );
    // The workspace root stays clean: no memory file is written beside the
    // user's sources.
    let stray: Vec<_> = read_dir(fx.workspace_root())
        .expect("read workspace")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".md"))
        .collect();
    assert!(
        stray.is_empty(),
        "promote writes no markdown into the workspace root: {stray:?}"
    );
    // Recall still finds the topic (under the project scope now).
    let listed: Vec<_> = fx
        .provider
        .list_memories()
        .into_iter()
        .filter(|m| m.key == "rule-test-naming")
        .collect();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].scope, MemoryScope::Project);
}

/// promote_memory is idempotent: a second call on an already-promoted topic
/// does not error and leaves exactly one index pointer for the key.
#[test]
fn test_promote_idempotent_project_topic() {
    let fx = ThreeScopeFixture::new();
    let entry = fx.entry("rule-idem", "Rule sentence here.\nWhy: x.\n");
    fx.provider.add(entry).expect("seed");
    fx.provider
        .promote_memory("rule-idem")
        .expect("first promote");
    fx.provider
        .promote_memory("rule-idem")
        .expect("second promote (idempotent)");
    let index = read_to_string(fx.project_root.join("MEMORY.md")).expect("project index");
    let count = index
        .lines()
        .filter(|l| l.starts_with("- rule-idem "))
        .count();
    assert_eq!(
        count, 1,
        "exactly one index pointer after two promotes: {index}"
    );
}

/// demote_memory is the reverse: moves the topic project -> auto. The rule
/// leaves the always-on project index, so the topic is recall-on-demand
/// only.
#[test]
fn test_demote_reverses_promote() {
    let fx = ThreeScopeFixture::new();
    let entry = fx.entry("rule-rev", "Always-on rule.\nWhy: y.\n");
    fx.provider.add(entry).expect("seed auto");
    fx.provider.promote_memory("rule-rev").expect("promote");
    let index_after_promote =
        read_to_string(fx.project_root.join("MEMORY.md")).expect("project index");
    assert!(
        index_after_promote.contains("- rule-rev "),
        "promote puts the key in the always-on index: {index_after_promote}"
    );
    fx.provider.demote_memory("rule-rev").expect("demote");
    // Topic moved project -> auto.
    assert!(
        !fx.project_root.join("rule-rev.md").is_file(),
        "project topic gone"
    );
    assert!(
        fx.auto_root.join("rule-rev.md").is_file(),
        "auto topic present"
    );
    // The always-on index no longer carries the key.
    let index = read_to_string(fx.project_root.join("MEMORY.md")).expect("project index");
    assert!(
        !index.contains("- rule-rev "),
        "the key leaves the always-on index: {index}"
    );
    // Recall finds it under the auto scope now.
    let listed: Vec<_> = fx
        .provider
        .list_memories()
        .into_iter()
        .filter(|m| m.key == "rule-rev")
        .collect();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].scope, MemoryScope::Auto);
}

/// demote_memory on a topic already in auto is a no-op: the file stays put
/// and the always-on index never gains the key.
#[test]
fn test_demote_strips_when_auto() {
    let fx = ThreeScopeFixture::new();
    let entry = fx.entry("rule-decay", "Decayed rule.\nWhy: z.\n");
    fx.provider.add(entry).expect("seed auto");
    fx.provider
        .promote_memory("rule-decay")
        .expect("promote into the project scope");
    // Simulate a manual topic move back to auto (e.g. user moved the file).
    rename(
        fx.project_root.join("rule-decay.md"),
        fx.auto_root.join("rule-decay.md"),
    )
    .expect("manual move");
    fx.provider.demote_memory("rule-decay").expect("demote");
    assert!(
        fx.auto_root.join("rule-decay.md").is_file(),
        "the topic stays in auto"
    );
    assert!(
        !fx.project_root.join("rule-decay.md").is_file(),
        "no project copy is left behind"
    );
}

/// promote_memory returns NotFound when the topic is in neither root, so
/// the caller (the dream) can react rather than silently no-op.
#[test]
fn test_promote_missing_not_found() {
    let fx = ThreeScopeFixture::new();
    let err = fx
        .provider
        .promote_memory("absent-rule")
        .expect_err("missing topic");
    assert!(
        matches!(err, MemoryError::NotFound),
        "NotFound for absent topic, got {err:?}"
    );
}

/// demote_missing_returns_not_found
#[test]
fn test_demote_missing_not_found() {
    let fx = ThreeScopeFixture::new();
    let err = fx
        .provider
        .demote_memory("absent-rule")
        .expect_err("missing topic");
    assert!(
        matches!(err, MemoryError::NotFound),
        "NotFound for absent topic, got {err:?}"
    );
}

/// record_gate_violation increments gate_violations for a key (signal B:
/// a PreToolUse gate denied a call on that rule). The counter accumulates
/// across calls and persists to the sidecar; the dream reads it to nominate
/// rules for promotion. A key with a path separator is rejected so a
/// malicious deny reason cannot poison the sidecar.
#[test]
fn test_gate_violation_accumulates() {
    let root = temp_dir().join(format!(
        "scope-flow-gate-{}-{}",
        id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));
    create_dir_all(&root).expect("mkdir");
    let p = MarkdownMemoryProvider::new(root.clone());
    p.record_gate_violation("test-naming");
    p.record_gate_violation("test-naming");
    p.record_gate_violation("test-naming");
    let stats = p.read_recall_stats();
    let by_key: HashMap<&str, &MemoryRecallStats> =
        stats.iter().map(|s| (s.key.as_str(), s)).collect();
    let entry = by_key["test-naming"];
    assert_eq!(entry.gate_violations, 3, "three violations accumulate to 3");
    assert_eq!(
        entry.recall_hits, 0,
        "gate violations do not touch recall_hits"
    );
    // A bad key (path separator) is rejected — no orphan stats row.
    p.record_gate_violation("../escape");
    let still = p.read_recall_stats();
    assert!(
        still.iter().all(|s| s.key != ".."),
        "no orphan row for a rejected key"
    );
    remove_dir_all(&root).ok();
}

/// promote_memory when the key exists in BOTH roots: the project copy is
/// the explicit source of truth, so the auto copy is dropped (no silent
/// overwrite of the project content). Closes the H1 data-loss regression
/// the adversarial verify flagged.
#[test]
fn test_promote_roots_keeps_project() {
    let fx = ThreeScopeFixture::new();
    // Seed a project-scope copy (the explicit entry) + a competing auto
    // copy that would shadow it by newest-mtime.
    let proj_entry = fx.entry("rule-both", "Project rule sentence.\nWhy: explicit.\n");
    fx.provider
        .add_in_scope(proj_entry, MemoryScope::Project)
        .expect("project seed");
    let auto_entry = fx.entry("rule-both", "Auto copy competing sentence.\nWhy: shadow.\n");
    fx.provider.add(auto_entry).expect("auto seed");
    // Pre-condition: both roots have the topic.
    assert!(fx.project_root.join("rule-both.md").is_file());
    assert!(fx.auto_root.join("rule-both.md").is_file());
    fx.provider.promote_memory("rule-both").expect("promote");
    // The project copy (the explicit source of truth) survives.
    let project_text =
        read_to_string(fx.project_root.join("rule-both.md")).expect("project copy survives");
    assert!(
        project_text.contains("Project rule sentence."),
        "project content preserved (not overwritten by the auto copy): {project_text}"
    );
    // The auto copy is gone (no competing shadow).
    assert!(
        !fx.auto_root.join("rule-both.md").is_file(),
        "auto copy dropped so it cannot shadow the project copy"
    );
    // Recall resolves the key to the project copy, not the dropped one.
    let listed: Vec<_> = fx
        .provider
        .list_memories()
        .into_iter()
        .filter(|m| m.key == "rule-both")
        .collect();
    assert_eq!(listed.len(), 1, "one surviving copy");
    assert_eq!(listed[0].scope, MemoryScope::Project);
}

/// An explicit scope is the user's own statement, so a newer automatic write
/// with the same key cannot replace it. The automatic writer runs on its own
/// schedule and would otherwise win every merge by being the most recent
/// edit, silently discarding what the user promoted.
#[test]
fn test_scope_beats_newer_auto() {
    let fx = ThreeScopeFixture::new();
    let project_entry = fx.entry("rule-priority", "Promoted rule sentence.\nWhy: explicit.\n");
    fx.provider
        .add_in_scope(project_entry, MemoryScope::Project)
        .expect("project seed");
    // An automatic copy of the same key, written afterwards so its mtime is
    // strictly newer than the project copy's.
    let auto_entry = fx.entry("rule-priority", "Automatic rewrite.\nWhy: newer.\n");
    fx.provider.add(auto_entry).expect("auto seed");
    let project_path = fx.project_root.join("rule-priority.md");
    let auto_path = fx.auto_root.join("rule-priority.md");
    // Stamp the project copy an hour into the past so the automatic copy is
    // strictly newer. Without an explicit stamp the two writes land in the
    // same second and the comparison would pass for the wrong reason.
    let older = SystemTime::now() - Duration::from_secs(3600);
    OpenOptions::new()
        .write(true)
        .open(&project_path)
        .expect("open project copy")
        .set_modified(older)
        .expect("stamp project copy");
    let project_mtime = metadata(&project_path)
        .expect("project meta")
        .modified()
        .expect("project mtime");
    let auto_mtime = metadata(&auto_path)
        .expect("auto meta")
        .modified()
        .expect("auto mtime");
    assert!(
        auto_mtime > project_mtime,
        "the automatic copy must be strictly newer for this to prove anything"
    );

    let listed: Vec<_> = fx
        .provider
        .list_memories()
        .into_iter()
        .filter(|m| m.key == "rule-priority")
        .collect();
    assert_eq!(listed.len(), 1, "one merged row per key");
    assert_eq!(
        listed[0].scope,
        MemoryScope::Project,
        "the explicit scope wins even though the automatic copy is newer"
    );

    let shown = fx.provider.show_memory("rule-priority").expect("body");
    assert!(
        shown.content.contains("Promoted rule sentence."),
        "recall returns the promoted body, not the automatic rewrite: {}",
        shown.content
    );

    // The prompt index is built from this same ordering, so the promoted
    // rule keeps its place at the head instead of being pushed out by the
    // automatic writer's churn.
    let first = fx
        .provider
        .list_memories()
        .into_iter()
        .next()
        .expect("a first row");
    assert_eq!(
        first.key, "rule-priority",
        "the explicit-scope row sorts ahead of the automatic one"
    );
}
