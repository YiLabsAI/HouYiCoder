//! Memory provider helpers. The MemoryProvider trait and its payload types
//! (MemoryEntry, MemoryError, MemorySource) live in the foundation layers:
//! the trait in the ports crate, the payload types in the context crate, so
//! neither ports nor the engine depends on this impl crate. This module keeps
//! the keyword-ranking helpers shared by all in-process providers (tokenize,
//! hit_count).

/// Split a query into lowercase keywords for lexical recall.
///
/// Space-separated scripts keep their word tokens. CJK runs (Han, Hiragana,
/// Katakana, Hangul) carry no word boundaries, so a two-character sliding
/// window of bigrams is generated alongside the full run — a memory whose
/// description shares any two adjacent characters with the query then ranks,
/// while the full run still wins an exact entity match. Tokens shorter than
/// two characters are dropped: a single character matches too much and
/// carries little signal.
pub(crate) fn tokenize(query: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut buf_cjk = false;
    for c in query.chars() {
        if !c.is_alphanumeric() {
            flush(&mut buf, buf_cjk, &mut out);
            buf_cjk = false;
            continue;
        }
        let cjk = is_cjk(c);
        if cjk != buf_cjk && !buf.is_empty() {
            flush(&mut buf, buf_cjk, &mut out);
        }
        buf_cjk = cjk;
        buf.push(c);
    }
    flush(&mut buf, buf_cjk, &mut out);
    out
}

/// Whether a query carries word content at all. A single CJK char or a
/// one-letter word is a real question even though it is too short to become
/// a keyword, so the rank still offers its candidates to the semantic stage
/// at score zero; an empty or punctuation-only query carries no signal and
/// ranks nothing.
pub(crate) fn has_word_content(query: &str) -> bool {
    query.chars().any(|c| c.is_alphanumeric())
}

/// Emit a finished run. A CJK run contributes its bigrams plus the full
/// string when the run is longer than two characters (for a two-character
/// run the single bigram already equals the full string, so the extra token
/// would only double-count). A space-separated run contributes only the full
/// token, and only when it is at least two characters.
fn flush(buf: &mut String, cjk: bool, out: &mut Vec<String>) {
    if buf.is_empty() {
        return;
    }
    let lower = buf.to_ascii_lowercase();
    let len = lower.chars().count();
    if len >= 2 {
        if cjk {
            push_bigrams(&lower, out);
            if len > 2 {
                out.push(lower);
            }
        } else {
            out.push(lower);
        }
    }
    buf.clear();
}

/// Slide a two-character window over a CJK run so a memory sharing any two
/// adjacent characters with the query ranks, without the false-positives a
/// single-character match would bring.
fn push_bigrams(s: &str, out: &mut Vec<String>) {
    let chars: Vec<char> = s.chars().collect();
    for w in chars.windows(2) {
        out.push(w.iter().collect());
    }
}

/// Whether a character belongs to a script without word boundaries: the CJK
/// unified ideographs and extensions, Japanese kana, and Korean hangul.
fn is_cjk(c: char) -> bool {
    matches!(
        c,
        '\u{3400}'..='\u{4DBF}'
        | '\u{4E00}'..='\u{9FFF}'
        | '\u{F900}'..='\u{FAFF}'
        | '\u{3040}'..='\u{309F}'
        | '\u{30A0}'..='\u{30FF}'
        | '\u{AC00}'..='\u{D7AF}'
    )
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
mod tests;
