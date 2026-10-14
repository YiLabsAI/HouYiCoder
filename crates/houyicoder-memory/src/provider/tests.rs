//! Tests for this module.

use super::*;

#[test]
fn test_tokenize_splits_and_lowercases() {
    let kw = tokenize("The FOX jumps");
    assert_eq!(kw, vec!["the", "fox", "jumps"]);
}

#[test]
fn test_tokenize_drops_single_chars() {
    let kw = tokenize("a fox b");
    assert_eq!(kw, vec!["fox"]);
}

#[test]
fn test_hit_count_counts_distinct() {
    let kw = tokenize("fox hound");
    assert_eq!(hit_count("the fox and the hound", &kw), 2);
    assert_eq!(hit_count("only the fox", &kw), 1);
    assert_eq!(hit_count("nothing here", &kw), 0);
}

#[test]
fn test_tokenize_cjk_run() {
    // A two-character CJK run yields one bigram (which equals the full
    // run, so the full token is not emitted twice).
    let run = "\u{90E8}\u{7F72}";
    let kw = tokenize(run);
    assert!(kw.contains(&run.to_string()));
    assert_eq!(
        kw.iter().filter(|k| *k == run).count(),
        1,
        "the bigram and full run are not double-counted"
    );
}

#[test]
fn test_tokenize_cjk_long_run() {
    let run = "\u{90E8}\u{7F72}\u{670D}\u{52A1}";
    let kw = tokenize(run);
    assert!(kw.contains(&"\u{90E8}\u{7F72}".to_string()));
    assert!(kw.contains(&"\u{7F72}\u{670D}".to_string()));
    assert!(kw.contains(&"\u{670D}\u{52A1}".to_string()));
    assert!(kw.contains(&run.to_string()));
}

#[test]
fn test_tokenize_cjk_from_latin() {
    // A mixed run breaks at the script boundary so each side keeps its
    // own matching shape: the CJK part bigrams, the latin part stays
    // whole.
    let kw = tokenize("\u{90E8}\u{7F72}gate");
    assert!(kw.contains(&"\u{90E8}\u{7F72}".to_string()));
    assert!(kw.contains(&"gate".to_string()));
    assert!(!kw.iter().any(|k| k.contains('\u{7F72}') && k.contains('g')));
}

#[test]
fn test_hit_count_cjk_overlap() {
    // A memory whose description shares a bigram with the query ranks,
    // even without an exact entity match.
    let kw = tokenize("\u{90E8}\u{7F72}gate");
    assert!(hit_count("\u{90E8}\u{7F72}\u{670D}\u{52A1}", &kw) >= 1);
    assert_eq!(hit_count("\u{65E0}\u{5173}\u{5185}\u{5BB9}", &kw), 0);
}

#[test]
fn test_cjk_exact_outranks() {
    // A memory holding the full run matches every bigram plus the full
    // token, so it outscores one sharing only a single bigram. This is
    // the ranking invariant the rerank layer will build on.
    let kw = tokenize("\u{90E8}\u{7F72}\u{670D}\u{52A1}");
    let exact = hit_count("\u{90E8}\u{7F72}\u{670D}\u{52A1}", &kw);
    let partial = hit_count("\u{90E8}\u{7F72}\u{5176}\u{4ED6}", &kw);
    assert!(
        exact > partial,
        "exact {exact} should outrank partial {partial}"
    );
}
