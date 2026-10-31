//! Word-level diff between two single lines, for the structured-diff
//! renderer's inline word highlighting. A presentation-layer concern (the
//! green/red bar is the line-level diff; the darker word background is the
//! inline refinement), so it lives in the TUI crate backed by similar's
//! unicode-word segmentation rather than in the core diff module (which
//! produces the line-level unified diff the renderer parses).

use similar::DiffTag;
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
    let old_tokens = tokenize(old);
    let new_tokens = tokenize(new);
    let diff = TextDiff::from_slices(&old_tokens, &new_tokens);
    let mut old_gone = vec![false; old_tokens.len()];
    let mut new_gone = vec![false; new_tokens.len()];
    for op in diff.ops() {
        if op.tag() == DiffTag::Equal {
            continue;
        }
        for i in op.old_range() {
            old_gone[i] = true;
        }
        for i in op.new_range() {
            new_gone[i] = true;
        }
    }
    tune_to_code_boundaries(&old_tokens, &mut old_gone);
    tune_to_code_boundaries(&new_tokens, &mut new_gone);
    assemble(&old_tokens, &old_gone, &new_tokens, &new_gone)
}

/// Split a line into code tokens: identifier runs, whitespace runs, and one
/// punctuation character each.
///
/// A word splitter treats an identifier and the punctuation touching it as
/// separate words, so a diff over words can cut a call apart in the middle:
/// removing an argument from cmd.arg(x).arg(y) produced a fragment that began
/// with an ampersand and ended with an unbalanced closing paren. Punctuation as
/// its own token is what lets the boundary correction below see the structure
/// the words alone hide.
fn tokenize(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut class = 0u8;
    for (i, c) in text.char_indices() {
        let next = if c.is_alphanumeric() || c == '_' {
            1
        } else if c.is_whitespace() {
            2
        } else {
            // Punctuation is always its own token: an argument list reads as
            // opener, contents, closer, and grouping two of those together
            // hides the seam the boundary correction has to find.
            3
        };
        let split = next != class && i > start;
        let after_punct = class == 3 && i > start;
        if split || after_punct {
            out.push(&text[start..i]);
            start = i;
        }
        class = next;
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

/// Whether a token opens or closes a group.
fn bracket_step(token: &str) -> i32 {
    match token {
        "(" | "[" | "{" => 1,
        ")" | "]" | "}" => -1,
        _ => 0,
    }
}

/// Trim a marked run back to a bracket-balanced, boundary-aligned fragment.
///
/// Token-level diffing places the seam wherever the shortest edit falls, which
/// for a removed call sits inside the call: the marked run started on the
/// argument and ended on the next call's opener. The reader then sees a
/// fragment with an unbalanced bracket at one end, which reads as corruption
/// rather than as a removal.
///
/// Trimming rather than widening is what keeps the split honest: widening pulls
/// kept tokens into the marked run, so the unchanged text would stop adding up
/// to the line. Every trim moves a token back to the kept side, and the kept
/// side plus the marked side still reconstructs the original exactly.
fn tune_to_code_boundaries(tokens: &[&str], gone: &mut [bool]) {
    let mut i = 0;
    while i < tokens.len() {
        if !gone[i] {
            i += 1;
            continue;
        }
        let mut end = i + 1;
        while end < tokens.len() && gone[end] {
            end += 1;
        }
        trim_run(tokens, gone, i, end);
        i = end;
    }
}

/// Trim one marked run to a bracket-balanced fragment, in place.
///
/// Balance is judged on every prefix, not on the total: a run holding a closer
/// and then an opener adds up to zero while still reading as a fragment that
/// began inside one call and ended inside another. The corrections below are
/// applied in the order the text is read, so each one fixes the end that
/// actually dangles.
fn trim_run(tokens: &[&str], gone: &mut [bool], start: usize, end: usize) {
    let mut start = start;
    let mut end = end;
    // A trailing opener has no closer inside the run, so it belongs to the
    // text after it. Dropping it first means the left correction below sees
    // the run the reader will actually read.
    while end > start + 1 && bracket_step(tokens[end - 1]) > 0 {
        gone[end - 1] = false;
        end -= 1;
    }
    // A leading closer means the run began inside a group. Extend left until
    // that group is whole, then take the name the group belonged to, so the
    // reader sees the call that went rather than its argument alone.
    let mut depth = 0;
    for token in &tokens[start..end] {
        depth += bracket_step(token);
    }
    while depth < 0 && start > 0 {
        start -= 1;
        depth += bracket_step(tokens[start]);
    }
    while start > 0 && bracket_step(tokens[start]) > 0 && start >= 2 {
        let named = tokens[start - 1]
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_');
        if named && matches!(tokens[start - 2], "." | "::") {
            start -= 2;
        } else {
            break;
        }
    }
    // The mirror case on the right: a trailing chain dot plus its name were
    // matched into the run only because the following call repeats the same
    // shape. Handing them back leaves the run ending on the call it removed.
    while end >= start + 3
        && matches!(tokens[end - 2], "." | "::")
        && tokens[end - 1]
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_')
    {
        gone[end - 1] = false;
        gone[end - 2] = false;
        end -= 2;
    }
    for flag in &mut gone[start..end] {
        *flag = true;
    }
}

/// Coalesce the two marked sets into the ordered part sequence the renderer
/// walks: shared runs, removals, and additions.
fn assemble(
    old_tokens: &[&str],
    old_gone: &[bool],
    new_tokens: &[&str],
    new_gone: &[bool],
) -> Vec<WordDiffPart> {
    let mut out: Vec<WordDiffPart> = Vec::new();
    let push = |text: &str, added: bool, removed: bool, out: &mut Vec<WordDiffPart>| {
        if text.is_empty() {
            return;
        }
        match out.last_mut() {
            Some(last) if last.added == added && last.removed == removed => {
                last.value.push_str(text);
            }
            _ => out.push(WordDiffPart {
                added,
                removed,
                value: text.to_string(),
            }),
        }
    };
    // Walk both sides in lockstep over the tokens neither side lost, which is
    // the order the reader sees: the line as it was, with its changes marked.
    let (mut i, mut j) = (0usize, 0usize);
    while i < old_tokens.len() || j < new_tokens.len() {
        if i < old_tokens.len() && old_gone[i] {
            push(old_tokens[i], false, true, &mut out);
            i += 1;
        } else if j < new_tokens.len() && new_gone[j] {
            push(new_tokens[j], true, false, &mut out);
            j += 1;
        } else if i < old_tokens.len() {
            push(old_tokens[i], false, false, &mut out);
            i += 1;
            j += 1;
        } else {
            push(new_tokens[j], true, false, &mut out);
            j += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests;
