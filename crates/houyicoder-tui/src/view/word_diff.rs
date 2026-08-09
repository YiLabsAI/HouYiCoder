//! Word-level diff between two single lines, for the structured-diff
//! renderer's inline word highlighting. A presentation-layer concern (the
//! green/red bar is the line-level diff; the darker word background is the
//! inline refinement), so it lives in the TUI crate backed by similar's
//! unicode-word segmentation rather than in the core diff module (which
//! produces the line-level unified diff the renderer parses).

/// One part of a word-level diff (a run of words all added, all removed, or
/// unchanged). A diff-part shape so the renderer can
/// apply a darker, more-saturated background to just the changed words. Equal
/// runs carry the shared text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WordDiffPart {
    pub added: bool,
    pub removed: bool,
    pub value: String,
}

/// Word-level diff of two single-line texts (an old line + its paired new
/// line), backed by similar's unicode-word segmentation. Returns a sequence
/// of parts (added / removed / equal) covering the whole line, so a small
/// inline edit surfaces as a few changed words rather than two whole-line
/// bars. The change-ratio threshold that gates whether to USE this (a
/// 0.4 change-threshold) lives in the renderer, not here.
pub fn word_diff(old: &str, new: &str) -> Vec<WordDiffPart> {
    let diff = similar::TextDiff::from_unicode_words(old, new);
    diff.ops()
        .iter()
        .flat_map(|op| diff.iter_inline_changes(op))
        .map(|c| {
            let (added, removed) = match c.tag() {
                similar::ChangeTag::Insert => (true, false),
                similar::ChangeTag::Delete => (false, true),
                similar::ChangeTag::Equal => (false, false),
            };
            let value: String = c.iter_strings_lossy().map(|(_, s)| s.to_string()).collect();
            WordDiffPart {
                added,
                removed,
                value,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
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
}
