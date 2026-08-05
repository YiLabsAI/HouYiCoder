//! Memory list/show projection: core memory types to the wire form. Split
//! out of projection.rs so that file stays under the size gate. The wire
//! carries the source as a lowercase label string (not the enum) so the
//! protocol crate stays free of the context types.

use houyicoder_context::{MemoryEntry, MemorySummary};
use houyicoder_protocol::frontend::memory::{MemoryDetail, MemorySummaryEntry, ToggleState};

/// Project the frontmatter-only memory summaries to the wire list form. The
/// listing path read no bodies, so this is a pure field copy plus the source
/// label.
pub(crate) fn project_memory_list(summaries: Vec<MemorySummary>) -> Vec<MemorySummaryEntry> {
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

/// Project one memory's full body to the wire form, or None when the key was
/// absent. Drops the token estimate (a render-time concern, not a wire one).
pub(crate) fn project_memory_entry(entry: Option<MemoryEntry>) -> Option<MemoryDetail> {
    entry.map(|e| MemoryDetail {
        key: e.key,
        content: e.content,
        source: e.source.as_label().to_string(),
        description: e.description,
        mtime_secs: e.mtime_secs,
    })
}

/// Project the toggle pair to the wire snapshot. A pure field copy so the
/// /memory pane renders the on/off rows without importing the config crate.
pub(crate) fn project_toggle_state(auto_memory: bool, auto_dream: bool) -> ToggleState {
    ToggleState {
        auto_memory,
        auto_dream,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_context::{MemoryEntry, MemoryScope, MemorySource, MemorySummary};

    /// The list projection maps the source enum to its wire label + carries the
    /// key, description, mtime. Pins the wire mapping the /memory list rides.
    #[test]
    fn test_list_maps_source_label() {
        let summaries = vec![
            MemorySummary::new(
                "build-gate",
                "make check must stay green",
                MemorySource::Project,
                MemoryScope::Project,
                0,
            ),
            MemorySummary::new(
                "user-pref",
                "prefer dark mode",
                MemorySource::User,
                MemoryScope::User,
                99,
            ),
        ];
        let wire = project_memory_list(summaries);
        assert_eq!(wire.len(), 2);
        assert_eq!(wire[0].key, "build-gate");
        assert_eq!(wire[0].source, "project");
        assert_eq!(wire[0].scope, "project", "scope flows to wire");
        assert_eq!(wire[1].source, "user");
        assert_eq!(wire[1].scope, "user", "scope flows to wire");
        assert_eq!(wire[1].mtime_secs, 99);
    }

    /// The show projection maps one entry's full body + drops the token
    /// estimate (a render-time concern, not a wire one). None passes through.
    #[test]
    fn test_show_entry_drops_tokens() {
        let entry = MemoryEntry::new(
            "build-gate",
            "make check must stay green",
            MemorySource::Project,
        )
        .with_meta("the build must pass", 42);
        let wire = project_memory_entry(Some(entry)).expect("Some");
        assert_eq!(wire.key, "build-gate");
        assert_eq!(wire.content, "make check must stay green");
        assert_eq!(wire.source, "project");
        assert_eq!(wire.description, "the build must pass");
        assert_eq!(wire.mtime_secs, 42);
        assert!(project_memory_entry(None).is_none(), "None passes through");
    }
}
