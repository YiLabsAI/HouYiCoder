//! Word-level diff between two single lines, for the structured-diff
//! renderer's inline word highlighting. A presentation-layer concern (the
//! green/red bar is the line-level diff; the darker word background is the
//! inline refinement), so it lives in the TUI crate backed by similar's
//! unicode-word segmentation rather than in the core diff module (which
//! produces the line-level unified diff the renderer parses).

use similar::ChangeTag;
use similar::TextDiff;

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
    let diff = TextDiff::from_unicode_words(old, new);
    diff.ops()
        .iter()
        .flat_map(|op| diff.iter_inline_changes(op))
        .map(|c| {
            let (added, removed) = match c.tag() {
                ChangeTag::Insert => (true, false),
                ChangeTag::Delete => (false, true),
                ChangeTag::Equal => (false, false),
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
mod tests;
