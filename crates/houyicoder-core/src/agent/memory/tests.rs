//! Index rendering: one heading per type, and scope priority deciding what fits.

use super::*;

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
