//! Tests for the marked lines a before-and-after pair renders as.

use ratatui::style::Color;
use ratatui::text::Line;

use super::changed_lines;

/// The text of a line, without its mark.
fn text_of(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

/// Both sides are marked, the earlier one first, so a change reads as what
/// left and what replaced it.
#[test]
fn test_changes_mark_both_sides() {
    let lines = changed_lines(Some("old"), Some("new"));
    let shown: Vec<String> = lines.iter().map(text_of).collect();
    assert_eq!(shown, vec!["-old".to_string(), "+new".to_string()]);
}

/// A created file has no earlier version, so nothing is marked as removed.
#[test]
fn test_creation_adds_lines() {
    let lines = changed_lines(None, Some("written"));
    let shown: Vec<String> = lines.iter().map(text_of).collect();
    assert_eq!(shown, vec!["+written".to_string()]);
}

/// Multi-line sides keep one line per mark, in the order each side holds.
#[test]
fn test_changes_keep_each_line() {
    let lines = changed_lines(Some("a\nb"), Some("c"));
    let shown: Vec<String> = lines.iter().map(text_of).collect();
    assert_eq!(
        shown,
        vec!["-a".to_string(), "-b".to_string(), "+c".to_string()]
    );
}

/// One changed line in a longer file marks that line alone. The unchanged
/// lines beside it are context, and the stretches further out collapse to a
/// count instead of being marked as if they had moved.
#[test]
fn test_single_change_keeps_context() {
    let old = "1\n2\n3\n4\n5\n6\n7\n8\n";
    let new = "1\n2\n3\nX\n5\n6\n7\n8\n";
    let lines = changed_lines(Some(old), Some(new));
    let shown: Vec<String> = lines.iter().map(text_of).collect();
    assert_eq!(
        shown,
        vec![
            "  … 1 unchanged line …".to_string(),
            " 2".to_string(),
            " 3".to_string(),
            "-4".to_string(),
            "+X".to_string(),
            " 5".to_string(),
            " 6".to_string(),
            "  … 2 unchanged lines …".to_string(),
        ]
    );
    assert_eq!(
        lines[1].spans[0].style.fg,
        Some(Color::DarkGray),
        "an unchanged line is dim so it does not compete with the change"
    );
}

/// A trailing newline ends the last line, it does not start an empty one. Both
/// sides ending the same way must compare equal rather than show a blank line
/// as a change.
#[test]
fn test_trailing_newline_no_phantom() {
    let lines = changed_lines(Some("a\nb\n"), Some("a\nb\n"));
    let shown: Vec<String> = lines.iter().map(text_of).collect();
    assert_eq!(shown, vec!["  … 2 unchanged lines …".to_string()]);
    assert!(
        !shown.iter().any(|line| line.trim().is_empty()),
        "no phantom empty line: {shown:?}"
    );
}

/// A side that holds nothing and a side that is absent both read as the other
/// side's lines alone, so a creation and an emptying name their whole content.
#[test]
fn test_absent_side_keeps_other() {
    let removed: Vec<String> = changed_lines(Some("gone\n"), None)
        .iter()
        .map(text_of)
        .collect();
    assert_eq!(removed, vec!["-gone".to_string()]);
}
