//! The paste store: what it keeps inline, what it stores behind a token, and
//! what the token expands back to.

use super::*;

#[test]
fn test_small_paste_inline() {
    let mut s = PasteStore::default();
    assert_eq!(s.ingest("hi"), "hi");
    assert!(s.entries.is_empty());
}

#[test]
fn test_large_paste_placeholder() {
    let mut s = PasteStore::default();
    let big = "a".repeat(1000);
    let token = s.ingest(&big);
    // 0 newlines => no +lines suffix; 1-indexed id.
    assert_eq!(token, "[Pasted text #1]");
    assert_eq!(s.entries.len(), 1);
}

#[test]
fn test_multi_line_paste_placeholder() {
    let mut s = PasteStore::default();
    let text = "line1\nline2\nline3\nline4";
    let token = s.ingest(text);
    // 3 newlines => +3 lines (not 4); 1-indexed id.
    assert_eq!(token, "[Pasted text #1 +3 lines]");
}

#[test]
fn test_expand_replaces_tokens() {
    let mut s = PasteStore::default();
    let big = "a".repeat(1000);
    let token = s.ingest(&big);
    let input = format!("prefix {} suffix", token);
    assert_eq!(
        PasteStore::expand(&input, &s),
        format!("prefix {} suffix", big)
    );
}

#[test]
fn test_two_pastes_distinct_content() {
    let mut s = PasteStore::default();
    let a = format!("first {}", "x".repeat(900));
    let b = format!("second {}", "y".repeat(900));
    let ta = s.ingest(&a);
    let tb = s.ingest(&b);
    assert_ne!(ta, tb, "tokens must differ");
    let sent_a = PasteStore::expand(&ta, &s);
    let sent_b = PasteStore::expand(&tb, &s);
    assert_eq!(sent_a, a);
    assert_eq!(sent_b, b);
    assert_ne!(sent_a, sent_b, "second send must not be first content");
}

#[test]
fn test_multiline_paste_keeps_newlines() {
    let mut s = PasteStore::default();
    let text = "line1\nline2\nline3\nline4\nline5";
    let token = s.ingest(text);
    let sent = PasteStore::expand(&token, &s);
    assert_eq!(sent, text);
    assert_eq!(sent.lines().count(), 5, "newlines must survive expand");
}

#[test]
fn test_expand_unknown_id_kept() {
    let s = PasteStore::default();
    assert_eq!(
        PasteStore::expand("[Pasted text #9 +1 lines]", &s),
        "[Pasted text #9 +1 lines]"
    );
}

#[test]
fn test_crlf_normalized_to_lf() {
    let mut s = PasteStore::default();
    // Windows \r\n line endings: 3 \r\n = 3 \n after normalization.
    let text = "line1\r\nline2\r\nline3\r\nline4";
    let token = s.ingest(text);
    assert!(token.contains("+3 lines"), "CRLF count: {token}");
    // The stored + expanded text must have \n only, no \r.
    let expanded = PasteStore::expand(&token, &s);
    assert!(!expanded.contains('\r'), "CR leaked: {expanded:?}");
    assert_eq!(expanded, "line1\nline2\nline3\nline4");
}

#[test]
fn test_lone_cr_becomes_newline() {
    let mut s = PasteStore::default();
    // Old-Mac \r-only: 3 \r = 3 \n after normalization.
    let text = "line1\rline2\rline3\rline4";
    let token = s.ingest(text);
    assert!(token.contains("+3 lines"), "CR count: {token}");
    let expanded = PasteStore::expand(&token, &s);
    assert_eq!(expanded, "line1\nline2\nline3\nline4");
}

#[test]
fn test_id_increments_across_pastes() {
    let mut s = PasteStore::default();
    let a = "x".repeat(900);
    let b = "y".repeat(900);
    let ta = s.ingest(&a);
    let tb = s.ingest(&b);
    assert!(ta.contains("#1"), "first id: {ta}");
    assert!(tb.contains("#2"), "second id: {tb}");
    // Store retains both entries (not cleared).
    assert!(!s.is_empty());
}

mod submit;
