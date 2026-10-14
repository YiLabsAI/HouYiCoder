//! Paste-as-reference: a large paste becomes a compact placeholder token in
//! the input box while the full text lives in a side table; on submit the
//! placeholders are expanded back to the real text. Keeps the input box
//! readable for a huge paste and avoids pushing megabytes through the layout
//! wrap math.

use std::time::Instant;

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

/// The paste chunks accumulated across multiple bracketed-paste events
/// before the 50ms gap flush decides whether to ingest them as one paste.
/// One owner because the buffer text and its last-chunk timestamp are
/// drained and restored together on each flush check: a flat field per
/// value invited a flush that drained the buffer but left the timestamp,
/// or vice versa, so the next check saw a timestamp with no buffer to
/// flush.
#[derive(Debug, Default)]
pub(crate) struct PendingPaste {
    /// The accumulated chunk text, built up across Paste events until the
    /// gap flush ingests it into the paste side table.
    pub(crate) buffer: Option<String>,
    /// The timestamp of the last received chunk, for the gap-based flush.
    pub(crate) last: Option<Instant>,
}

/// Extract the numeric id from a [Pasted text #N ...] token.
fn parse_id(token: &str) -> Option<usize> {
    let inner = token.strip_prefix("[Pasted text #")?.strip_suffix(']')?;
    let id_part = inner.split(' ').next()?;
    id_part.parse().ok()
}

#[cfg(test)]
mod tests;
