//! Tests for the scope-flow operations: add_in_scope, promote_memory, and
//! demote_memory. Verifies the file-op sequence the design pins (topic move
//! + carrier merge / strip + index rebuild), idempotency, and the
//!   MED-1 closure (a project-scope refresh no longer shadows the explicit
//!   entry with a competing auto copy).

use super::*;
use houyicoder_context::{MemoryEntry, MemoryScope, MemorySource};

/// A three-root provider rooted at a temp workspace, mirroring the
/// composition-root layout: user / project / auto roots under a workspace
/// dir, plus the project memory file (agent.md) at the workspace root.
struct ThreeScopeFixture {
    user_root: PathBuf,
    project_root: PathBuf,
    auto_root: PathBuf,
    workspace: PathBuf,
    provider: MarkdownMemoryProvider,
}

impl ThreeScopeFixture {
    fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let workspace =
            std::env::temp_dir().join(format!("scope-flow-{seq}-{}", std::process::id(),));
        // Layout matches the composition root: the project memory root sits
        // two directories below the workspace root, so the project memory
        // file at <workspace>/agent.md resolves via parent().parent().
        let user_root = workspace.join("user-memory");
        let project_root = workspace.join(".config").join("memory");
        let auto_root = workspace.join("auto-memory");
        std::fs::create_dir_all(&user_root).expect("mkdir user");
        std::fs::create_dir_all(&project_root).expect("mkdir project");
        std::fs::create_dir_all(&auto_root).expect("mkdir auto");
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

    fn agent_md(&self) -> PathBuf {
        self.workspace.join("agent.md")
    }

    fn entry(&self, key: &str, content: &str) -> MemoryEntry {
        MemoryEntry::new(key, content, MemorySource::Feedback).with_meta(format!("{key} hook"), 0)
    }
}

impl Drop for ThreeScopeFixture {
    fn drop(&mut self) {
        drop(std::fs::remove_dir_all(&self.workspace));
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

/// promote_memory moves a topic from auto into project and merges the rule
/// sentence into the project memory file (agent.md). The topic is still
/// recallable (now under the project scope), and the rule is always-on.
#[test]
fn test_promote_moves_and_merges() {
    let fx = ThreeScopeFixture::new();
    // Seed an auto-scope topic.
    let entry = fx.entry(
        "rule-test-naming",
        "Test names use verb-object.\nWhy: clarity.\nHow: foo_bar_baz.\n",
    );
    fx.provider.add(entry).expect("seed auto");
    // Pre-condition: agent.md does not exist yet.
    assert!(!fx.agent_md().exists());
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
    // agent.md created with the rule sentence as the first content line
    // (after the header).
    let carrier = std::fs::read_to_string(fx.agent_md()).expect("read carrier");
    assert!(
        carrier.contains("Test names use verb-object."),
        "rule sentence in carrier: {carrier}"
    );
    assert!(
        !carrier.contains("How: foo_bar_baz."),
        "only the rule sentence (not the how-to-apply prose) merged: {carrier}"
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
/// does not duplicate the carrier line + does not error.
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
    let carrier = std::fs::read_to_string(fx.agent_md()).expect("carrier");
    let count = carrier
        .lines()
        .filter(|l| l.trim() == "Rule sentence here.")
        .count();
    assert_eq!(
        count, 1,
        "carrier has exactly one rule line after two promotes"
    );
}

/// demote_memory is the reverse: moves the topic project -> auto and strips
/// the rule sentence from the agent.md carrier file. The rule leaves the
/// always-on prefix, the topic is recall-on-demand only.
#[test]
fn test_demote_reverses_promote() {
    let fx = ThreeScopeFixture::new();
    let entry = fx.entry("rule-rev", "Always-on rule.\nWhy: y.\n");
    fx.provider.add(entry).expect("seed auto");
    fx.provider.promote_memory("rule-rev").expect("promote");
    let carrier_after_promote = std::fs::read_to_string(fx.agent_md()).expect("carrier");
    assert!(carrier_after_promote.contains("Always-on rule."));
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
    // Carrier line stripped.
    let carrier = std::fs::read_to_string(fx.agent_md()).expect("carrier post-demote");
    assert!(
        !carrier.contains("Always-on rule."),
        "rule line stripped: {carrier}"
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

/// demote_memory on a topic already in auto still strips the carrier line
/// (idempotent on the scope side, but the carrier line is removed either way
/// so the always-on prefix drops the rule).
#[test]
fn test_demote_strips_when_auto() {
    let fx = ThreeScopeFixture::new();
    let entry = fx.entry("rule-decay", "Decayed rule.\nWhy: z.\n");
    fx.provider.add(entry).expect("seed auto");
    fx.provider
        .promote_memory("rule-decay")
        .expect("promote to add the carrier line");
    // Simulate a manual topic move back to auto (e.g. user moved the file).
    std::fs::rename(
        fx.project_root.join("rule-decay.md"),
        fx.auto_root.join("rule-decay.md"),
    )
    .expect("manual move");
    // demote should still strip the carrier line.
    fx.provider.demote_memory("rule-decay").expect("demote");
    let carrier = std::fs::read_to_string(fx.agent_md()).expect("carrier");
    assert!(
        !carrier.contains("Decayed rule."),
        "carrier line stripped even when topic already in auto: {carrier}"
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
    let root = std::env::temp_dir().join(format!(
        "scope-flow-gate-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));
    std::fs::create_dir_all(&root).expect("mkdir");
    let p = MarkdownMemoryProvider::new(root.clone());
    p.record_gate_violation("test-naming");
    p.record_gate_violation("test-naming");
    p.record_gate_violation("test-naming");
    let stats = p.read_recall_stats();
    let by_key: std::collections::HashMap<&str, &MemoryRecallStats> =
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
    std::fs::remove_dir_all(&root).ok();
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
    let project_text = std::fs::read_to_string(fx.project_root.join("rule-both.md"))
        .expect("project copy survives");
    assert!(
        project_text.contains("Project rule sentence."),
        "project content preserved (not overwritten by the auto copy): {project_text}"
    );
    // The auto copy is gone (no competing shadow).
    assert!(
        !fx.auto_root.join("rule-both.md").is_file(),
        "auto copy dropped so it cannot shadow the project copy"
    );
    // The carrier line is the project rule (the explicit source).
    let carrier = std::fs::read_to_string(fx.agent_md()).expect("carrier");
    assert!(
        carrier.contains("Project rule sentence."),
        "carrier carries the project rule: {carrier}"
    );
    assert!(
        !carrier.contains("Auto copy competing sentence."),
        "carrier does not carry the auto copy: {carrier}"
    );
}
