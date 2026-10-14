//! Map core memory types to the wire form. The wire carries the source as
//! a lowercase label string (not the enum) so the protocol crate stays free
//! of the context types.

use houyicoder_context::{MemoryEntry, MemorySummary};
use houyicoder_protocol::frontend::memory::{MemoryDetail, MemorySummaryEntry, ToggleState};

/// Map frontmatter-only memory summaries to the wire list form. Pure field
/// copy plus the source label.
pub(crate) fn map_memory_list(summaries: Vec<MemorySummary>) -> Vec<MemorySummaryEntry> {
    summaries
        .into_iter()
        .map(|s| MemorySummaryEntry {
            key: s.key,
            description: s.description,
            source: s.source.as_label().to_string(),
            scope: s.scope.as_label().to_string(),
            mtime_secs: s.mtime_secs,
        })
        .collect()
}

/// Map one memory's full body to the wire form, or None when the key was
/// absent. Drops the token estimate (render-time concern, not wire).
pub(crate) fn map_memory_entry(entry: Option<MemoryEntry>) -> Option<MemoryDetail> {
    entry.map(|e| MemoryDetail {
        key: e.key,
        content: e.content,
        source: e.source.as_label().to_string(),
        description: e.description,
        mtime_secs: e.mtime_secs,
    })
}

/// Map the toggle pair to the wire snapshot.
pub(crate) fn map_toggle_state(auto_memory: bool, auto_dream: bool) -> ToggleState {
    ToggleState {
        auto_memory,
        auto_dream,
    }
}

#[cfg(test)]
mod tests;
