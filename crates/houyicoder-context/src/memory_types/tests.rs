//! Tests for this module.

use super::*;

#[test]
fn test_tokens_for_rounds_up() {
    assert_eq!(tokens_for(""), 0);
    assert_eq!(tokens_for("ab"), 1);
    assert_eq!(tokens_for("abcd"), 1);
    assert_eq!(tokens_for("abcde"), 2);
}

#[test]
fn test_entry_new_computes_tokens() {
    let e = MemoryEntry::new("k", "abcd", MemorySource::User);
    assert_eq!(e.tokens, 1);
    let e = MemoryEntry::new("k", "abcdef", MemorySource::Project);
    assert_eq!(e.tokens, 2);
}

#[test]
fn test_source_label_round_trip() {
    for s in [
        MemorySource::User,
        MemorySource::Feedback,
        MemorySource::Project,
        MemorySource::Reference,
    ] {
        assert_eq!(MemorySource::from_label(s.as_label()), Some(s));
    }
    assert!(MemorySource::from_label("unknown").is_none());
}

#[test]
fn test_source_label_case_insensitive() {
    assert_eq!(MemorySource::from_label("USER"), Some(MemorySource::User));
}

#[test]
fn test_age_days_clamps_future() {
    // Same instant -> 0; one day ago -> 1; five days -> 5; future clamps to 0.
    assert_eq!(memory_age_days(1000, 1000), 0);
    assert_eq!(memory_age_days(1000, 1000 + 86_400), 1);
    assert_eq!(memory_age_days(1000, 1000 + 5 * 86_400), 5);
    assert_eq!(memory_age_days(2000, 1000), 0, "future mtime clamps to 0");
}

#[test]
fn test_age_label_branches() {
    assert_eq!(memory_age_label(0), "today");
    assert_eq!(memory_age_label(1), "yesterday");
    assert_eq!(memory_age_label(5), "5 days ago");
    assert_eq!(memory_age_label(47), "47 days ago");
}

#[test]
fn test_freshness_text_threshold() {
    // Fresh (today/yesterday) -> no caveat; older -> caveat naming the age.
    assert!(memory_freshness_text(0).is_empty());
    assert!(memory_freshness_text(1).is_empty());
    let caveat = memory_freshness_text(9);
    assert!(
        caveat.contains("9 days old"),
        "caveat names the age: {caveat}"
    );
    assert!(
        caveat.contains("point-in-time"),
        "caveat flags staleness: {caveat}"
    );
}

#[test]
fn test_with_meta_description_mtime() {
    let e = MemoryEntry::new("k", "body", MemorySource::User).with_meta("a hook line", 1234);
    assert_eq!(e.description, "a hook line");
    assert_eq!(e.mtime_secs, 1234);
    // The bare constructor leaves the meta fields empty for callers that
    // never attach them (e.g. fact extraction that has no file mtime yet).
    let bare = MemoryEntry::new("k", "body", MemorySource::User);
    assert!(bare.description.is_empty());
    assert_eq!(bare.mtime_secs, 0);
}
