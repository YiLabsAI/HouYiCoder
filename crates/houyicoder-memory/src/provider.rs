//! Memory provider utilities. The MemoryProvider trait and its payload types
//! (MemoryEntry, MemoryError, MemoryType) live in the foundation layers:
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
        let cjk = houyicoder_context::is_cjk(c);
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

/// Count how many distinct keywords occur in the text via case-insensitive
/// substring match. Shared by all keyword-based providers.
pub(crate) fn hit_count(text: &str, keywords: &[String]) -> u32 {
    let lower = text.to_ascii_lowercase();
    keywords
        .iter()
        .filter(|k| lower.contains(k.as_str()))
        .count() as u32
}

pub(crate) struct PreparedKeywords {
    items: Vec<(String, bool)>,
}

impl PreparedKeywords {
    pub(crate) fn new(keywords: Vec<String>) -> Self {
        let items = keywords
            .into_iter()
            .take(64)
            .map(|k| {
                let short = is_short_latin(&k);
                (k, short)
            })
            .collect();
        Self { items }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.items.len()
    }

    pub(crate) fn matched_bits(&self, text: &str) -> u64 {
        let lower = text.to_ascii_lowercase();
        self.matched_bits_lower(&lower)
    }

    pub(crate) fn matched_bits_lower(&self, lower: &str) -> u64 {
        let mut mask = 0u64;
        for (idx, (keyword, short_latin)) in self.items.iter().enumerate() {
            let matched = if *short_latin {
                contains_word(lower, keyword)
            } else {
                lower.contains(keyword.as_str())
            };
            if matched {
                mask |= 1u64 << idx;
            }
        }
        mask
    }
}

/// Closed-class English function words and CJK question particles that appear
/// across unrelated queries and hyphenated slugs without carrying topic signal.
pub(crate) fn is_common_word(token: &str) -> bool {
    matches!(
        token,
        "an" | "and"
            | "are"
            | "as"
            | "at"
            | "be"
            | "been"
            | "but"
            | "by"
            | "can"
            | "could"
            | "did"
            | "do"
            | "does"
            | "for"
            | "from"
            | "had"
            | "has"
            | "have"
            | "how"
            | "if"
            | "in"
            | "into"
            | "is"
            | "it"
            | "its"
            | "not"
            | "of"
            | "on"
            | "or"
            | "over"
            | "should"
            | "so"
            | "than"
            | "that"
            | "the"
            | "their"
            | "them"
            | "then"
            | "there"
            | "these"
            | "they"
            | "this"
            | "to"
            | "was"
            | "were"
            | "what"
            | "when"
            | "where"
            | "which"
            | "who"
            | "why"
            | "will"
            | "with"
            | "would"
            | "you"
            | "your"
            | "什么"
            | "怎么"
            | "为什"
            | "为什么"
            | "可以"
            | "需要"
            | "多少"
    )
}

/// Whether the keyword is a short Latin word, the case that needs a word
/// boundary. A CJK keyword is always two characters, but it is not Latin and
/// keeps substring matching.
fn is_short_latin(keyword: &str) -> bool {
    keyword.len() <= 3 && keyword.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Whether the lowercased text carries the keyword as a whole word.
fn contains_word(lower: &str, keyword: &str) -> bool {
    let mut from = 0;
    while let Some(found) = lower[from..].find(keyword) {
        let start = from + found;
        let end = start + keyword.len();
        let before_ok = lower[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        let after_ok = lower[end..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric());
        if before_ok && after_ok {
            return true;
        }
        from = start + keyword.len().max(1);
    }
    false
}

#[cfg(test)]
mod tests;
