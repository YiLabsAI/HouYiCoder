use super::*;

#[test]
fn test_tab_header() {
    let line = tab_header(1, &[(0, "First"), (1, "Second")]);
    let text: String = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    assert_eq!(text, "  First  [Second]");
}

#[test]
fn test_hint_pairs() {
    let line = key_hint(&[("Up/Down", "select"), ("Enter", "open"), ("Esc", "close")]);
    assert!(line.spans.len() >= 6, "3 pairs x 2 spans + 2 separators");
}

#[test]
fn test_hint_single() {
    let line = key_hint(&[("Esc", "back")]);
    assert_eq!(line.spans.len(), 2, "one key + one action");
}

#[test]
fn test_hint_empty() {
    let line = key_hint(&[]);
    assert!(line.spans.is_empty(), "no pairs = empty line");
}
