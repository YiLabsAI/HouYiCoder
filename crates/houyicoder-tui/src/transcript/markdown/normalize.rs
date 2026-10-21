//! Rewrite the prose parts of a markdown document before the parser reads
//! them.
//!
//! Two shapes of model output need this. A bold run written against Chinese
//! text and punctuation is not a valid left-flanking delimiter under the
//! CommonMark rules, so the parser keeps the asterisks as text; anchoring the
//! run with a zero-width space on its inner side makes the flanking test pass
//! without changing what the reader sees. An enclosed numeral such as a
//! circled digit is East Asian ambiguous: fonts commonly draw it wider than
//! the single cell every width calculation assigns it, so the glyph covers the
//! character after it and a table's columns stop lining up. Both rewrites skip
//! fenced blocks and inline code, which must reach the reader unchanged.

use std::borrow::Cow;

/// The anchor inserted inside a bold run. Unicode classifies it as a format
/// character, not whitespace and not punctuation, which is what makes the
/// flanking test accept a run that touches Chinese text.
const ANCHOR: char = '\u{200b}';

/// Rewrite the prose of a markdown document: pair bold runs and replace
/// enclosed numerals with their parenthesized ASCII form.
///
/// The text is returned unchanged when it holds neither shape. A fenced block
/// is copied verbatim, and an inline code span is recognised by its backtick
/// run length, including a span that opens on one line and closes on a later
/// line of the same paragraph.
pub(super) fn prose(text: &str) -> Cow<'_, str> {
    if !text.contains("**") && !text.chars().any(|c| circled_number(c).is_some()) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 16);
    // Output offsets of the runs that need an anchor, collected while the
    // text is built and applied afterwards: inserting into a string shifts
    // every offset after the insertion point.
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    let mut open: Option<usize> = None;
    let mut fence: Option<(char, usize)> = None;
    // The run length of an inline code span still waiting for its closing
    // run, when that run is on a later line.
    let mut code: Option<usize> = None;
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        let rest = &text[offset..];
        offset += line.len();
        if let Some((marker, length)) = fence {
            if closes_fence(line, marker, length) {
                fence = None;
            }
            out.push_str(line);
            continue;
        }
        if let Some((marker, length)) = fence_marker(line) {
            fence = Some((marker, length));
            open = None;
            out.push_str(line);
            continue;
        }
        if starts_block(line) {
            open = None;
        }
        scan_line(line, rest, &mut out, &mut open, &mut pairs, &mut code);
        if line.trim().is_empty() {
            open = None;
        }
    }
    if pairs.is_empty() && out == text {
        return Cow::Borrowed(text);
    }
    for (after_open, before_close) in pairs.into_iter().rev() {
        out.insert(before_close, ANCHOR);
        out.insert(after_open, ANCHOR);
    }
    Cow::Owned(out)
}

/// Drop the anchors a bold run carries, so the rendered line and the copied
/// text hold what the reader was meant to see.
pub(super) fn without_anchor(text: &str) -> Cow<'_, str> {
    if !text.contains(ANCHOR) {
        return Cow::Borrowed(text);
    }
    Cow::Owned(text.replace(ANCHOR, ""))
}

/// A fence opening line: up to three spaces of indent, then three or more
/// backticks or tildes.
fn fence_marker(line: &str) -> Option<(char, usize)> {
    let body = line.trim_start_matches(' ');
    if line.len() - body.len() > 3 {
        return None;
    }
    let marker = body.chars().next()?;
    if marker != '`' && marker != '~' {
        return None;
    }
    let length = body.chars().take_while(|c| *c == marker).count();
    (length >= 3).then_some((marker, length))
}

/// Whether a line closes the fence it is read inside. A closing fence carries
/// the opening marker and at least as many of them, and nothing else that
/// matters.
fn closes_fence(line: &str, marker: char, length: usize) -> bool {
    let body = line.trim_start_matches(' ');
    if line.len() - body.len() > 3 {
        return false;
    }
    let run = body.chars().take_while(|c| *c == marker).count();
    run >= length && body[run..].trim().is_empty()
}

/// Whether the line starts a block of its own, where an emphasis span cannot
/// continue from the line before.
fn starts_block(line: &str) -> bool {
    let body = line.trim_start();
    if body.is_empty() {
        return true;
    }
    let mut chars = body.chars();
    match chars.next() {
        Some('#') | Some('>') | Some('|') => true,
        Some('-') | Some('+') | Some('*') => chars.next().is_some_and(|c| c == ' '),
        Some(c) if c.is_ascii_digit() => {
            let rest = body.trim_start_matches(|c: char| c.is_ascii_digit());
            rest.starts_with(". ") || rest.starts_with(") ")
        }
        _ => false,
    }
}

/// Rewrite one line that is not part of a fenced block.
///
/// The document text from this line onward is needed for one decision: a run
/// with no closing run on this line is the start of a code span only when a
/// closing run exists further on, and is literal text otherwise.
fn scan_line(
    line: &str,
    rest: &str,
    out: &mut String,
    open: &mut Option<usize>,
    pairs: &mut Vec<(usize, usize)>,
    code: &mut Option<usize>,
) {
    let bytes = line.as_bytes();
    let mut i = 0usize;
    if let Some(length) = *code {
        match find_run(line, length) {
            Some(at) => {
                out.push_str(&line[..at + length]);
                i = at + length;
                *code = None;
            }
            None => {
                out.push_str(line);
                return;
            }
        }
    }
    while i < line.len() {
        let rest_line = &line[i..];
        let c = rest_line
            .chars()
            .next()
            .expect("a char starts at a boundary");
        if c == '`' && !escaped(bytes, i) {
            let run = run_length(rest_line, '`');
            match find_run(&rest_line[run..], run) {
                Some(at) => {
                    out.push_str(&rest_line[..run + at + run]);
                    i += run + at + run;
                    continue;
                }
                None if find_run(&paragraph(rest)[i + run..], run).is_some() => {
                    // The span closes on a later line: the rest of this line
                    // is its opening run and content, copied as it stands.
                    out.push_str(rest_line);
                    *code = Some(run);
                    return;
                }
                // With no closing run anywhere, the run is literal text and
                // is read like any other prose character.
                None => {
                    out.push_str(&rest_line[..run]);
                    i += run;
                    continue;
                }
            }
        }
        if c == '*' && !escaped(bytes, i) {
            let run = run_length(rest_line, '*');
            if run == 2 {
                let prev = out.chars().next_back();
                let can_open = opens_bold(rest_line, prev);
                // A run attached to the word before it closes a span the way
                // the parser reads it. A run that cannot open has no other
                // reading either, which is how a closing run written after a
                // space still closes.
                let can_close = prev.is_some_and(|p| !p.is_whitespace()) || !can_open;
                match *open {
                    Some(at) if can_close && out.len() > at => {
                        pairs.push((at, out.len()));
                        *open = None;
                    }
                    // This run is the opener the author meant; the pending one
                    // had no closer of its own.
                    Some(_) if can_open => *open = Some(out.len() + 2),
                    Some(_) => {}
                    None if can_open => *open = Some(out.len() + 2),
                    None => {}
                }
            }
            out.push_str(&rest_line[..run]);
            i += run;
            continue;
        }
        if c == '|' && !escaped(bytes, i) {
            // A table cell is a block of its own: a run cannot close across
            // the separator.
            *open = None;
            out.push('|');
            i += 1;
            continue;
        }
        match circled_number(c) {
            Some(n) => {
                out.push('(');
                out.push_str(&n.to_string());
                out.push(')');
            }
            None => out.push(c),
        }
        i += c.len_utf8();
    }
}

/// The paragraph the line belongs to, from the line's start. A code span
/// cannot cross a blank line or the start of another block, so a closing run
/// beyond either does not close it.
fn paragraph(rest: &str) -> &str {
    let mut offset = 0usize;
    let mut first = true;
    for line in rest.split_inclusive('\n') {
        if !first && (line.trim().is_empty() || fence_marker(line).is_some() || starts_block(line))
        {
            break;
        }
        offset += line.len();
        first = false;
    }
    &rest[..offset]
}

/// How many times the character repeats at the start of the text.
fn run_length(text: &str, marker: char) -> usize {
    text.chars().take_while(|c| *c == marker).count()
}

/// The byte offset of a run of exactly this many backticks, which is what
/// closes a code span opened by a run of the same length.
fn find_run(text: &str, length: usize) -> Option<usize> {
    let mut i = 0usize;
    while i < text.len() {
        let c = text[i..].chars().next()?;
        if c == '`' {
            let run = run_length(&text[i..], '`');
            if run == length {
                return Some(i);
            }
            i += run;
            continue;
        }
        i += c.len_utf8();
    }
    None
}

/// Whether the byte at this offset is escaped by a backslash.
fn escaped(bytes: &[u8], at: usize) -> bool {
    let mut backslashes = 0usize;
    let mut i = at;
    while i > 0 && bytes[i - 1] == b'\\' {
        backslashes += 1;
        i -= 1;
    }
    backslashes % 2 == 1
}

/// Whether a bold run can open a span the parser needs help with.
///
/// A run followed by whitespace cannot open emphasis. A run attached to a word
/// on both sides is intraword emphasis, which the parser already accepts, so
/// anchoring it would only add bytes. Every other shape is one the flanking
/// rule may reject because the run touches punctuation, which is what the
/// anchor is for.
fn opens_bold(text: &str, prev: Option<char>) -> bool {
    let Some(next) = text.chars().nth(2) else {
        return false;
    };
    if next.is_whitespace() {
        return false;
    }
    let attached_left = prev.is_some_and(char::is_alphanumeric);
    !(attached_left && next.is_alphanumeric())
}

/// The number an enclosed numeral stands for, or nothing when the character
/// is not one. Only the enclosed forms whose glyphs are drawn wider than the
/// one cell their width reports are mapped.
fn circled_number(c: char) -> Option<u32> {
    let cp = c as u32;
    let n = match cp {
        0x24EA => 0,
        0x2460..=0x2473 => cp - 0x2460 + 1,
        0x24F5..=0x24FE => cp - 0x24F5 + 1,
        0x2776..=0x277F => cp - 0x2776 + 1,
        0x3251..=0x325F => cp - 0x3251 + 21,
        0x32B1..=0x32BF => cp - 0x32B1 + 36,
        _ => return None,
    };
    Some(n)
}

#[cfg(test)]
mod tests;
