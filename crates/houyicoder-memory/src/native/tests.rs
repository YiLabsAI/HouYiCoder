//! Tests for this module.

use super::*;
use houyicoder_context::MemoryType;

fn entry(key: &str, content: &str, source: MemoryType) -> MemoryEntry {
    MemoryEntry::new(key, content, source)
}

#[test]
fn test_rank_orders_by_relevance() {
    let provider = KeywordRecallProvider::with_entries(vec![
        entry("a", "the quick brown fox jumps", MemoryType::Project),
        entry("b", "a totally unrelated note about cats", MemoryType::User),
        entry("c", "fox sightings near the hen house", MemoryType::Project),
    ]);
    let out = provider.rank_candidates("fox brown", &HashSet::new());
    // The entry with both keywords ranks above the one with one.
    assert_eq!(out[0].key, "a");
    assert_eq!(out[0].score, 2);
    assert_eq!(out[1].key, "c");
    assert_eq!(out[1].score, 1);
    // The unrelated entry stays listed with a zero score, sorted last —
    // it is still a candidate the semantic stage may select.
    assert_eq!(out[2].key, "b");
    assert_eq!(out[2].score, 0);
}

/// Mirrors the markdown provider: an unsplittable query still offers
/// its rows at score zero, while punctuation-only ranks nothing.
#[test]
fn test_rank_unsplittable_offers_rows() {
    let provider = KeywordRecallProvider::with_entries(vec![entry(
        "tea",
        "how the team orders tea",
        MemoryType::Project,
    )]);
    let out = provider.rank_candidates("\u{8336}", &HashSet::new());
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].score, 0);
    assert!(provider.rank_candidates("!!!", &HashSet::new()).is_empty());
}

#[test]
fn test_rank_recency_breaks_tie() {
    let provider = KeywordRecallProvider::with_entries(vec![
        entry("a", "alpha fox", MemoryType::Project),
        entry("b", "bravo fox", MemoryType::Project),
        entry("c", "charlie fox", MemoryType::Project),
    ]);
    let out = provider.rank_candidates("fox", &HashSet::new());
    // Recency wins the tie: the newest single-hit entry comes first.
    let keys: Vec<&str> = out.iter().map(|h| h.key.as_str()).collect();
    assert_eq!(keys, vec!["c", "b", "a"]);
}

#[test]
fn test_rank_skips_surfaced() {
    let provider = KeywordRecallProvider::with_entries(vec![
        entry("a", "alpha fox", MemoryType::Project),
        entry("b", "bravo fox", MemoryType::Project),
    ]);
    let mut surfaced = HashSet::new();
    surfaced.insert("a".to_string());
    let out = provider.rank_candidates("fox", &surfaced);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].key, "b");
}

#[test]
fn test_rank_empty_store_returns() {
    let provider = KeywordRecallProvider::new();
    assert!(
        provider
            .rank_candidates("anything", &HashSet::new())
            .is_empty()
    );
}

#[test]
fn test_rank_empty_query_returns() {
    let provider = KeywordRecallProvider::with_entries(vec![entry("a", "fox", MemoryType::User)]);
    assert!(provider.rank_candidates("", &HashSet::new()).is_empty());
    assert!(provider.rank_candidates("   ", &HashSet::new()).is_empty());
}

#[test]
fn test_rank_is_case_insensitive() {
    let provider =
        KeywordRecallProvider::with_entries(vec![entry("a", "The FOX barks", MemoryType::User)]);
    let out = provider.rank_candidates("fox", &HashSet::new());
    assert_eq!(out[0].score, 1);
}

#[test]
fn test_rank_prefers_more_hits() {
    let provider = KeywordRecallProvider::with_entries(vec![
        entry("a", "fox fox fox", MemoryType::Project),
        entry("b", "fox and the hound", MemoryType::Project),
    ]);
    let out = provider.rank_candidates("fox hound", &HashSet::new());
    assert_eq!(out[0].key, "b");
    assert_eq!(out[1].key, "a");
}

#[test]
fn test_add_then_rank() {
    let provider = KeywordRecallProvider::new();
    provider
        .add(entry("k", "documented fox behavior", MemoryType::Feedback))
        .unwrap();
    let out = provider.rank_candidates("fox", &HashSet::new());
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].key, "k");
    assert_eq!(out[0].memory_type, MemoryType::Feedback);
}

#[test]
fn test_show_reads_back_entry() {
    let provider = KeywordRecallProvider::new();
    let e = entry("k", "alpha fox body", MemoryType::Project);
    let expected_tokens = e.tokens;
    provider.add(e).unwrap();
    let out = provider.show_memory("k").expect("stored entry reads back");
    assert_eq!(out.tokens, expected_tokens);
    assert_eq!(out.content, "alpha fox body");
    assert!(provider.show_memory("absent").is_none());
}
