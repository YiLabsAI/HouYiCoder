//! Paste-as-reference: a large paste becomes a compact placeholder token in
//! the input box while the full text lives in a side table; on submit the
//! placeholders are expanded back to the real text. Keeps the input box
//! readable for a huge paste and avoids pushing megabytes through the layout
//! wrap math.

const PASTE_THRESHOLD: usize = 800;
const PASTE_MAX_LINES: usize = 2;

/// Side table for pasted content that was replaced by a placeholder token in
/// the input box. IDs are 1-indexed and increment across the session (the
/// store is NOT cleared on submit, so IDs stay stable across the session); the vec
/// is 0-indexed so expand converts via id - 1.
#[derive(Default, Debug)]
pub struct PasteStore {
    entries: Vec<String>,
}

impl PasteStore {
    /// True when no pasted content is stashed.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Decide whether a paste is large enough to reference, store it if so,
    /// and return either the placeholder token or the raw text (small pastes
    /// go inline).
    pub fn ingest(&mut self, text: &str) -> String {
        // Normalize line endings: \r\n -> \n, lone \r -> \n, so the line
        // count and stored text are consistent regardless of the clipboard's
        // line-ending style (some apps/tmux send \r-only).
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        // Count line-BREAKS (\n occurrences), not line elements — a
        // 5-line text has 4 \n, so "+4 lines" not "+5".
        let lines = text.matches('\n').count();
        if text.len() <= PASTE_THRESHOLD && lines <= PASTE_MAX_LINES {
            return text;
        }
        let id = self.entries.len() + 1; // 1-indexed so the first paste is #1
        self.entries.push(text.clone());
        if lines == 0 {
            format!("[Pasted text #{}]", id)
        } else {
            format!("[Pasted text #{} +{} lines]", id, lines)
        }
    }

    /// Replace every [Pasted text #N ...] token in the input with the
    /// stored text. Unknown IDs are left as-is (defensive — a stale token
    /// from a prior session should not silently drop).
    pub fn expand(text: &str, store: &PasteStore) -> String {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find("[Pasted text #") {
            out.push_str(&rest[..start]);
            let after = &rest[start..];
            if let Some(end) = after.find(']') {
                let token = &after[..=end];
                if let Some(id) = parse_id(token) {
                    // Token id is 1-indexed; the vec is 0-indexed.
                    if let Some(full) = store.entries.get(id.saturating_sub(1)) {
                        out.push_str(full);
                    } else {
                        out.push_str(token);
                    }
                } else {
                    out.push_str(token);
                }
                rest = &after[end + 1..];
            } else {
                // no closing bracket; copy the rest verbatim
                out.push_str(after);
                break;
            }
        }
        out.push_str(rest);
        out
    }
}

/// Extract the numeric id from a [Pasted text #N ...] token.
fn parse_id(token: &str) -> Option<usize> {
    let inner = token.strip_prefix("[Pasted text #")?.strip_suffix(']')?;
    let id_part = inner.split(' ').next()?;
    id_part.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_small_paste_inline() {
        let mut s = PasteStore::default();
        assert_eq!(s.ingest("hi"), "hi");
        assert!(s.entries.is_empty());
    }

    #[test]
    fn test_large_paste_placeholder() {
        let mut s = PasteStore::default();
        let big = "a".repeat(1000);
        let token = s.ingest(&big);
        // 0 newlines => no +lines suffix; 1-indexed id.
        assert_eq!(token, "[Pasted text #1]");
        assert_eq!(s.entries.len(), 1);
    }

    #[test]
    fn test_multi_line_paste_placeholder() {
        let mut s = PasteStore::default();
        let text = "line1\nline2\nline3\nline4";
        let token = s.ingest(text);
        // 3 newlines => +3 lines (not 4); 1-indexed id.
        assert_eq!(token, "[Pasted text #1 +3 lines]");
    }

    #[test]
    fn test_expand_replaces_tokens() {
        let mut s = PasteStore::default();
        let big = "a".repeat(1000);
        let token = s.ingest(&big);
        let input = format!("prefix {} suffix", token);
        assert_eq!(
            PasteStore::expand(&input, &s),
            format!("prefix {} suffix", big)
        );
    }

    #[test]
    fn test_two_pastes_distinct_content() {
        let mut s = PasteStore::default();
        let a = format!("first {}", "x".repeat(900));
        let b = format!("second {}", "y".repeat(900));
        let ta = s.ingest(&a);
        let tb = s.ingest(&b);
        assert_ne!(ta, tb, "tokens must differ");
        let sent_a = PasteStore::expand(&ta, &s);
        let sent_b = PasteStore::expand(&tb, &s);
        assert_eq!(sent_a, a);
        assert_eq!(sent_b, b);
        assert_ne!(sent_a, sent_b, "second send must not be first content");
    }

    #[test]
    fn test_multiline_paste_keeps_newlines() {
        let mut s = PasteStore::default();
        let text = "line1\nline2\nline3\nline4\nline5";
        let token = s.ingest(text);
        let sent = PasteStore::expand(&token, &s);
        assert_eq!(sent, text);
        assert_eq!(sent.lines().count(), 5, "newlines must survive expand");
    }

    #[test]
    fn test_expand_unknown_id_kept() {
        let s = PasteStore::default();
        assert_eq!(
            PasteStore::expand("[Pasted text #9 +1 lines]", &s),
            "[Pasted text #9 +1 lines]"
        );
    }

    #[test]
    fn test_crlf_normalized_to_lf() {
        let mut s = PasteStore::default();
        // Windows \r\n line endings: 3 \r\n = 3 \n after normalization.
        let text = "line1\r\nline2\r\nline3\r\nline4";
        let token = s.ingest(text);
        assert!(token.contains("+3 lines"), "CRLF count: {token}");
        // The stored + expanded text must have \n only, no \r.
        let expanded = PasteStore::expand(&token, &s);
        assert!(!expanded.contains('\r'), "CR leaked: {expanded:?}");
        assert_eq!(expanded, "line1\nline2\nline3\nline4");
    }

    #[test]
    fn test_lone_cr_becomes_newline() {
        let mut s = PasteStore::default();
        // Old-Mac \r-only: 3 \r = 3 \n after normalization.
        let text = "line1\rline2\rline3\rline4";
        let token = s.ingest(text);
        assert!(token.contains("+3 lines"), "CR count: {token}");
        let expanded = PasteStore::expand(&token, &s);
        assert_eq!(expanded, "line1\nline2\nline3\nline4");
    }

    #[test]
    fn test_id_increments_across_pastes() {
        let mut s = PasteStore::default();
        let a = "x".repeat(900);
        let b = "y".repeat(900);
        let ta = s.ingest(&a);
        let tb = s.ingest(&b);
        assert!(ta.contains("#1"), "first id: {ta}");
        assert!(tb.contains("#2"), "second id: {tb}");
        // Store retains both entries (not cleared).
        assert!(!s.is_empty());
    }
}
