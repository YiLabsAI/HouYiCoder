//! Index rendering: one heading per type, and scope priority deciding what fits.

use super::*;
use houyicoder_api::agent_event::MemoryOperation;
use houyicoder_context::MemoryType;
use houyicoder_memory::{InMemoryBackend, MarkdownMemoryProvider};
use houyicoder_session::SessionStore;
use std::env::temp_dir;
use std::fs::{create_dir_all, remove_dir_all};
use std::path::PathBuf;
use std::process::id;
use std::sync::atomic::{AtomicU64, Ordering};

fn summary(key: &str, source: MemoryType, scope: MemoryScope) -> MemorySummary {
    MemorySummary::new(key, "one line", source, scope, 0)
}

/// One type heading per group, however many scopes carry that type.
#[test]
fn test_index_heading_once() {
    let rows = vec![
        summary("a", MemoryType::Feedback, MemoryScope::User),
        summary("b", MemoryType::Feedback, MemoryScope::Project),
        summary("c", MemoryType::Feedback, MemoryScope::Auto),
    ];
    let text = render_index(&rows);
    assert_eq!(text.matches("### feedback").count(), 1);
    assert_eq!(
        text.lines().filter(|line| line.starts_with("- ")).count(),
        3
    );
}

/// Scope priority decides what fits, so a preference the user wrote is not
/// starved by a large automatic set.
#[test]
fn test_index_user_rows_first() {
    let mut rows = vec![summary("user-row", MemoryType::Feedback, MemoryScope::User)];
    let long = "x".repeat(200);
    for i in 0..40 {
        rows.push(MemorySummary::new(
            format!("auto-{i}"),
            long.clone(),
            MemoryType::Feedback,
            MemoryScope::Auto,
            0,
        ));
    }
    let text = render_index(&rows);
    assert!(
        text.contains("user-row"),
        "the user row survives the budget"
    );
}

/// The budget stops the tail: a store whose rows exceed it yields fewer rows
/// than it holds.
#[test]
fn test_index_stops_at_budget() {
    let long = "x".repeat(200);
    let rows: Vec<MemorySummary> = (0..120)
        .map(|i| {
            MemorySummary::new(
                format!("k-{i}"),
                long.clone(),
                MemoryType::Feedback,
                MemoryScope::Auto,
                0,
            )
        })
        .collect();
    let text = render_index(&rows);
    let count = text.lines().filter(|line| line.starts_with("- ")).count();
    assert!(count < rows.len(), "the budget stops the tail: {count}");
    assert!(count > 0, "some rows still fit");
}

fn temp_roots(tag: &str) -> (PathBuf, Arc<MarkdownMemoryProvider>) {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let root = temp_dir().join(format!("core-mem-{tag}-{seq}-{}", id()));
    let user = root.join("user");
    let project = root.join("project");
    let auto = root.join("auto");
    for d in [&user, &project, &auto] {
        create_dir_all(d).expect("mkdir");
    }
    let provider = Arc::new(MarkdownMemoryProvider::new_multi(vec![user, project, auto]));
    (root, provider)
}

#[test]
fn test_active_rules_order() {
    let (root, provider) = temp_roots("active-rules");
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let runtime = MemoryRuntime::from_parts(
        store,
        Some(Arc::clone(&provider) as Arc<dyn MemoryProvider>),
        MemoryGates::new(true, true),
        None,
        None,
    );

    // Cold project feedback (0 violations, 0 hits, 1 occurrence) stays in the
    // compact index instead of consuming an active rule card.
    provider
        .add_in_scope(
            MemoryEntry::new("cold-proj", "Cold project note.", MemoryType::Feedback),
            MemoryScope::Project,
        )
        .expect("cold");
    // Updated once (occurrences = 2), 0 hits.
    provider
        .add_in_scope(
            MemoryEntry::new("twice-proj", "v1", MemoryType::Feedback),
            MemoryScope::Project,
        )
        .expect("v1");
    provider
        .add_in_scope(
            MemoryEntry::new("twice-proj", "Updated rule card.", MemoryType::Feedback),
            MemoryScope::Project,
        )
        .expect("v2");
    // Frequently recalled (hits = 4) must outrank occurrences = 2.
    provider
        .add_in_scope(
            MemoryEntry::new("recalled-proj", "High recall rule.", MemoryType::Feedback),
            MemoryScope::Project,
        )
        .expect("recalled");
    for _ in 0..4 {
        provider.record_recall_hits(&["recalled-proj".to_string()]);
    }
    // User feedback always qualifies.
    let long_latin = format!("User direct rule {} tailword", "part ".repeat(70));
    provider
        .add_in_scope(
            MemoryEntry::new("user-rule", long_latin, MemoryType::Feedback),
            MemoryScope::User,
        )
        .expect("user");

    let rules = runtime.format_active_rules().expect("active rules");
    assert!(!rules.contains("cold-proj"), "cold rule stays out: {rules}");
    assert!(rules.contains("- [recalled-proj] High recall rule."));
    assert!(rules.contains("- [twice-proj] Updated rule card."));
    assert!(rules.contains("- [user-rule] User direct rule"));
    assert!(
        !rules.contains("tailword"),
        "long rule card is truncated at word boundary"
    );
    let recall_pos = rules.find("recalled-proj").expect("recalled pos");
    let twice_pos = rules.find("twice-proj").expect("twice pos");
    assert!(
        recall_pos < twice_pos,
        "recall_hits outranks occurrences when violations tie:\n{rules}"
    );
    drop(remove_dir_all(&root));
}

#[tokio::test]
async fn test_gate_off_skips_reconcile() {
    let (root, provider) = temp_roots("gate-off");
    provider
        .add(MemoryEntry::new(
            "hot-auto",
            "Auto rule.",
            MemoryType::Feedback,
        ))
        .expect("seed auto");
    provider.record_gate_violation("hot-auto");
    provider.record_gate_violation("hot-auto");

    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let runtime = MemoryRuntime::from_parts(
        store,
        Some(Arc::clone(&provider) as Arc<dyn MemoryProvider>),
        MemoryGates::new(false, false),
        None,
        None,
    );
    runtime
        .fire_background::<fn() -> RewardSnapshot>(SessionId::new(), None)
        .await;
    runtime.join_background(Duration::from_secs(1)).await;

    assert!(
        root.join("auto").join("hot-auto.md").is_file(),
        "disabled auto_memory gate must never move files in background"
    );
    assert!(!root.join("project").join("hot-auto.md").exists());
    drop(remove_dir_all(&root));
}

#[tokio::test]
async fn test_reconcile_invalidates_rules() {
    let (root, provider) = temp_roots("active-inval");
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let runtime = MemoryRuntime::from_parts(
        store,
        Some(Arc::clone(&provider) as Arc<dyn MemoryProvider>),
        MemoryGates::new(true, true),
        None,
        None,
    );
    // Cache the initial empty active rules
    assert!(runtime.format_active_rules().is_none());

    // Add an auto feedback entry and give it 2 violations so it promotes to Project
    provider
        .add(MemoryEntry::new(
            "hot-rule",
            "Must format clean.",
            MemoryType::Feedback,
        ))
        .expect("seed auto");
    provider.record_gate_violation("hot-rule");
    provider.record_gate_violation("hot-rule");

    runtime
        .fire_background::<fn() -> RewardSnapshot>(SessionId::new(), None)
        .await;
    runtime.join_background(Duration::from_secs(1)).await;

    // After reconcile_scopes promotes the rule, the cached snapshot must have been invalidated
    let rules = runtime
        .format_active_rules()
        .expect("active rules must be populated after reconcile");
    assert!(
        rules.contains("- [hot-rule] Must format clean."),
        "promoted rule must be present without manual /clear:\n{rules}"
    );

    // Primary recorder: an auto-scope save preserves the snapshot, whereas an
    // explicit-scope save invalidates it immediately.
    let mut primary_runtime = MemoryRuntime::from_parts(
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))),
        Some(Arc::clone(&provider) as Arc<dyn MemoryProvider>),
        MemoryGates::new(true, true),
        None,
        None,
    );
    let recorder = primary_runtime.install_primary_recorder();
    let before = primary_runtime
        .format_active_rules()
        .expect("seeded active rules");
    provider
        .add_in_scope(
            MemoryEntry::new("fresh-user-rule", "Direct user rule.", MemoryType::Feedback),
            MemoryScope::User,
        )
        .expect("seed user rule");
    recorder.record("auto-note", MemoryOperation::Created, MemoryScope::Auto);
    primary_runtime.drain_primary_changes();
    assert_eq!(
        primary_runtime.format_active_rules().as_deref(),
        Some(before.as_str()),
        "ordinary auto saves keep the prompt snapshot stable"
    );
    recorder.record(
        "fresh-user-rule",
        MemoryOperation::Created,
        MemoryScope::User,
    );
    primary_runtime.drain_primary_changes();
    let refreshed = primary_runtime
        .format_active_rules()
        .expect("refreshed active rules");
    assert!(
        refreshed.contains("- [fresh-user-rule] Direct user rule."),
        "explicit-scope primary saves invalidate the snapshot immediately:\n{refreshed}"
    );
    drop(remove_dir_all(&root));
}

#[test]
fn test_runtime_archived_flow() {
    let (root, provider) = temp_roots("runtime-archive");
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let runtime = MemoryRuntime::from_parts(
        store,
        Some(Arc::clone(&provider) as Arc<dyn MemoryProvider>),
        MemoryGates::new(true, true),
        None,
        None,
    );
    provider
        .add(MemoryEntry::new("arc-k", "Arc body.", MemoryType::Project))
        .expect("seed");
    provider
        .archive_memory("arc-k", MemoryScope::Auto)
        .expect("archive");

    assert_eq!(runtime.list_archived().len(), 1);
    assert_eq!(
        runtime.show("arc-k").expect("show archived").content,
        "Arc body."
    );
    runtime.restore("arc-k", "auto").expect("restore");
    assert!(runtime.list_archived().is_empty());
    assert_eq!(runtime.list().len(), 1);

    provider
        .archive_memory("arc-k", MemoryScope::Auto)
        .expect("re-archive");
    runtime.forget("arc-k", "auto").expect("forget archived");
    assert!(runtime.list_archived().is_empty());
    drop(remove_dir_all(&root));
}

#[test]
fn test_restore_searches_scopes() {
    let (root, provider) = temp_roots("restore-scope");
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let runtime = MemoryRuntime::from_parts(
        store,
        Some(Arc::clone(&provider) as Arc<dyn MemoryProvider>),
        MemoryGates::new(true, true),
        None,
        None,
    );
    provider
        .add_in_scope(
            MemoryEntry::new("proj-arc", "Project arc body.", MemoryType::Project),
            MemoryScope::Project,
        )
        .expect("seed project memory");
    provider
        .archive_memory("proj-arc", MemoryScope::Project)
        .expect("archive into project");

    assert_eq!(runtime.list_archived().len(), 1);
    // Passing empty scope string must probe all scopes and find it in Project
    runtime
        .restore("proj-arc", "")
        .expect("restore without scope");
    assert!(runtime.list_archived().is_empty());
    assert_eq!(runtime.list().len(), 1);
    drop(remove_dir_all(&root));
}
