//! Recalled memories as model-facing context.
//!
//! The recall budget bounds how many characters one retrieval may return. The
//! rendered text carries the same untrusted framing every memory read does,
//! and each entry states its source, age, description, and content so the
//! model can judge freshness before relying on it.

use std::time::{SystemTime, UNIX_EPOCH};

use houyicoder_context::{MemoryEntry, memory_age_days, memory_age_label, memory_freshness_text};

/// Per-turn recall budget shared by retrieval and context assembly.
pub(crate) const MEMORY_RECALL_BUDGET: usize = 2000;

/// Render recalled memories as untrusted model context.
///
/// Entries include source, age, description, content, and a freshness warning
/// when current code should be rechecked.
pub(crate) fn render_recall_text(entries: &[MemoryEntry]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut text = String::from("<system-reminder>\n# Recalled memories\n");
    for entry in entries {
        let age_days = memory_age_days(entry.mtime_secs, now_secs);
        let age_label = memory_age_label(age_days);
        let header = if entry.description.is_empty() {
            format!(
                "- [{}] {} ({})\n",
                entry.source.as_label(),
                entry.key,
                age_label
            )
        } else {
            format!(
                "- [{}] {} ({}): {}\n",
                entry.source.as_label(),
                entry.key,
                age_label,
                entry.description
            )
        };
        text.push_str(&header);
        text.push_str(&entry.content);
        if !text.ends_with('\n') {
            text.push('\n');
        }
        let caveat = memory_freshness_text(age_days);
        if !caveat.is_empty() {
            text.push_str(&caveat);
            text.push('\n');
        }
    }
    text.push_str("</system-reminder>");
    text
}
