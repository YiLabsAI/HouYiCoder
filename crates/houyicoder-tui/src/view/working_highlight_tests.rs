use super::transcript::highlighted_line;

#[test]
fn test_highlighted_no_query_white() {
    let line = highlighted_line("hello world", "", false);
    assert_eq!(line.spans.len(), 1);
}

#[test]
fn test_highlighted_line_with_query() {
    let line = highlighted_line("find the bug in the bug", "bug", false);
    let joined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(joined, "find the bug in the bug");
    assert!(line.spans.len() >= 3);
}

#[test]
fn test_highlighted_line_case_insensitive() {
    let line = highlighted_line("the BUG is here", "bug", false);
    let joined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(joined.contains("BUG"));
}
