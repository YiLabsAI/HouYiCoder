//! Memory provider helpers. The MemoryProvider trait and its payload types
//! (MemoryEntry, MemoryError, MemorySource) live in the foundation layers:
//! the trait in the ports crate, the payload types in the context crate, so
//! neither ports nor the engine depends on this impl crate. This module keeps
//! the keyword-ranking helpers shared by the in-process providers (tokenize,
//! hit_count) plus the per-provider bookkeeping they need.

/// Split a query into lowercase alphanumeric keywords. Tokens shorter than
/// two characters are dropped: single-character splits match too much and
/// carry little signal. Shared by all keyword-based providers.
pub(crate) fn tokenize(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| s.len() >= 2)
        .map(|s| s.to_ascii_lowercase())
        .collect()
}

/// Count how many distinct keywords occur in the text via case-insensitive
/// substring match. Shared by all keyword-based providers.
pub(crate) fn hit_count(text: &str, keywords: &[String]) -> u32 {
    let lower = text.to_ascii_lowercase();
    keywords
        .iter()
        .filter(|k| lower.contains(k.as_str()))
        .count() as u32
}

#[cfg(test)]
mod tests {
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
}
