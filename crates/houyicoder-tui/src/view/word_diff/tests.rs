use super::*;

#[test]
fn test_single_token_change() {
    // "let x = 1" → "let x = 2": only the "1"/"2" token changes; the rest
    // is an equal run. Parts include a removed "1" + added "2".
    let parts = word_diff("let x = 1", "let x = 2");
    let removed: Vec<_> = parts.iter().filter(|p| p.removed).collect();
    let added: Vec<_> = parts.iter().filter(|p| p.added).collect();
    assert_eq!(removed.len(), 1);
    assert_eq!(added.len(), 1);
    assert_eq!(removed[0].value, "1");
    assert_eq!(added[0].value, "2");
}

#[test]
fn test_covers_full_line() {
    // Parts concatenated reconstruct the old line (equal + removed) and
    // the new line (equal + added).
    let parts = word_diff("fn foo(a: i32)", "fn foo(a: u32)");
    let old_rebuilt: String = parts
        .iter()
        .filter(|p| !p.added)
        .map(|p| p.value.as_str())
        .collect();
    let new_rebuilt: String = parts
        .iter()
        .filter(|p| !p.removed)
        .map(|p| p.value.as_str())
        .collect();
    assert_eq!(old_rebuilt, "fn foo(a: i32)");
    assert_eq!(new_rebuilt, "fn foo(a: u32)");
}

#[test]
fn test_identical_all_equal() {
    let parts = word_diff("same line", "same line");
    assert!(parts.iter().all(|p| !p.added && !p.removed));
    let joined: String = parts.iter().map(|p| p.value.as_str()).collect();
    assert_eq!(joined, "same line");
}

#[test]
fn test_empty_sides() {
    // Pure insertion: every part added, concatenated = the text.
    let parts = word_diff("", "new line");
    assert!(parts.iter().all(|p| p.added && !p.removed));
    let joined: String = parts.iter().map(|p| p.value.as_str()).collect();
    assert_eq!(joined, "new line");
    // Pure deletion: every part removed.
    let parts = word_diff("old line", "");
    assert!(parts.iter().all(|p| p.removed && !p.added));
    let joined: String = parts.iter().map(|p| p.value.as_str()).collect();
    assert_eq!(joined, "old line");
}
