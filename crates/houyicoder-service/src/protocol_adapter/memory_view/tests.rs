//! Tests for this module.

use super::*;
use houyicoder_context::{MemoryEntry, MemoryScope, MemorySource, MemorySummary};

/// The list mapping maps the source enum to its wire label + carries the
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
    let wire = map_memory_list(summaries);
    assert_eq!(wire.len(), 2);
    assert_eq!(wire[0].key, "build-gate");
    assert_eq!(wire[0].source, "project");
    assert_eq!(wire[0].scope, "project", "scope flows to wire");
    assert_eq!(wire[1].source, "user");
    assert_eq!(wire[1].scope, "user", "scope flows to wire");
    assert_eq!(wire[1].mtime_secs, 99);
}

/// The show mapping maps one entry's full body + drops the token
/// estimate (a render-time concern, not a wire one). None passes through.
#[test]
fn test_show_entry_drops_tokens() {
    let entry = MemoryEntry::new(
        "build-gate",
        "make check must stay green",
        MemorySource::Project,
    )
    .with_meta("the build must pass", 42);
    let wire = map_memory_entry(Some(entry)).expect("Some");
    assert_eq!(wire.key, "build-gate");
    assert_eq!(wire.content, "make check must stay green");
    assert_eq!(wire.source, "project");
    assert_eq!(wire.description, "the build must pass");
    assert_eq!(wire.mtime_secs, 42);
    assert!(map_memory_entry(None).is_none(), "None passes through");
}
