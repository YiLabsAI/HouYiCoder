//! Index rendering: one heading per type, and scope priority deciding what fits.

use super::*;
use houyicoder_context::MemorySource;
use houyicoder_memory::{InMemoryBackend, MarkdownMemoryProvider};
use houyicoder_session::SessionStore;
use std::env::temp_dir;
use std::fs::{create_dir_all, remove_dir_all};
use std::path::PathBuf;
use std::process::id;
use std::sync::atomic::{AtomicU64, Ordering};

fn summary(key: &str, source: MemorySource, scope: MemoryScope) -> MemorySummary {
    MemorySummary::new(key, "one line", source, scope, 0)
}

/// One type heading per group, however many scopes carry that type.
#[test]
fn test_index_heading_once() {
    let rows = vec![
        summary("a", MemorySource::Feedback, MemoryScope::User),
        summary("b", MemorySource::Feedback, MemoryScope::Project),
        summary("c", MemorySource::Feedback, MemoryScope::Auto),
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
    let mut rows = vec![summary(
        "user-row",
        MemorySource::Feedback,
        MemoryScope::User,
    )];
    let long = "x".repeat(200);
    for i in 0..40 {
        rows.push(MemorySummary::new(
            format!("auto-{i}"),
            long.clone(),
            MemorySource::Feedback,
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
                MemorySource::Feedback,
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
            MemoryEntry::new("cold-proj", "Cold project note.", MemorySource::Feedback),
            MemoryScope::Project,
        )
        .expect("cold");
    // Updated once (occurrences = 2), 0 hits.
    provider
        .add_in_scope(
            MemoryEntry::new("twice-proj", "v1", MemorySource::Feedback),
            MemoryScope::Project,
        )
        .expect("v1");
    provider
        .add_in_scope(
            MemoryEntry::new("twice-proj", "Updated rule card.", MemorySource::Feedback),
            MemoryScope::Project,
        )
        .expect("v2");
    // Frequently recalled (hits = 4) must outrank occurrences = 2.
    provider
        .add_in_scope(
            MemoryEntry::new("recalled-proj", "High recall rule.", MemorySource::Feedback),
            MemoryScope::Project,
        )
        .expect("recalled");
    for _ in 0..4 {
        provider.record_recall_hits(&["recalled-proj".to_string()]);
    }
    // User feedback always qualifies.
    provider
        .add_in_scope(
            MemoryEntry::new("user-rule", "User direct rule.", MemorySource::Feedback),
            MemoryScope::User,
        )
        .expect("user");

    let rules = runtime.format_active_rules().expect("active rules");
    assert!(!rules.contains("cold-proj"), "cold rule stays out: {rules}");
    assert!(rules.contains("- [recalled-proj] High recall rule."));
    assert!(rules.contains("- [twice-proj] Updated rule card."));
    assert!(rules.contains("- [user-rule] User direct rule."));
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
            MemorySource::Feedback,
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
