//! Tests for the shared memory index format.

use super::*;

/// Rows render as key, colon, description, under one heading per type in
/// the fixed group order.
#[test]
fn test_groups_and_row_format() {
    let text = render_memory_index_groups([
        (MemoryType::User, "pref", "answer briefly"),
        (MemoryType::Feedback, "rule", "run the gate"),
        (MemoryType::Project, "arch", "one host, many guests"),
    ]);
    assert_eq!(
        text,
        "### feedback\n- rule: run the gate\n### project\n- arch: one host, many guests\n\
         ### user\n- pref: answer briefly\n"
    );
    assert_eq!(memory_index_row("k", "d"), "- k: d");
}

/// An empty type emits no heading, so a sparse store does not carry
/// headings with nothing under them.
#[test]
fn test_empty_group_no_heading() {
    let text = render_memory_index_groups([(MemoryType::Project, "k", "d")]);
    assert!(!text.contains("### user"));
    assert_eq!(text.matches("###").count(), 1);
}
