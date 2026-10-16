//! Tests for the /context suggestions: which measurement crosses which band,
//! what the advice names, and the order a reader meets it in.

use super::super::suggestions_for;
use crate::records::SuggestionSeverity;
use houyicoder_protocol::frontend::context::{ContextBreakdown, ContextFileEntry, ToolFootprint};

/// A breakdown whose only content is the footprints and memory files given, so
/// a band crossing comes from the measurement under test rather than from a
/// background total.
fn breakdown(
    window: u32,
    footprints: Vec<ToolFootprint>,
    memory: Vec<ContextFileEntry>,
) -> ContextBreakdown {
    let total = footprints
        .iter()
        .fold(0u32, |sum, f| {
            sum.saturating_add(f.call_tokens + f.result_tokens)
        })
        .saturating_add(
            memory
                .iter()
                .fold(0u32, |sum, f| sum.saturating_add(f.tokens)),
        );
    ContextBreakdown {
        model: "test".into(),
        total_tokens: total,
        context_window: window,
        categories: Vec::new(),
        grid: Vec::new(),
        cache_breakpoint: None,
        memory_files: memory,
        skills: Vec::new(),
        compact_summary: None,
        cache_prefix_tokens: None,
        cache_hit_rate: None,
        tool_footprints: footprints,
        delegated: None,
    }
}

fn footprint(tool: &str, call_tokens: u32, result_tokens: u32) -> ToolFootprint {
    ToolFootprint {
        tool: tool.into(),
        calls: 1,
        call_tokens,
        result_tokens,
    }
}

fn memory_file(label: &str, tokens: u32) -> ContextFileEntry {
    ContextFileEntry {
        label: label.into(),
        tokens,
    }
}

fn titles(breakdown: &ContextBreakdown) -> Vec<String> {
    suggestions_for(breakdown)
        .into_iter()
        .map(|s| s.title)
        .collect()
}

#[test]
fn test_near_capacity_warns() {
    assert!(
        titles(&breakdown(100_000, Vec::new(), Vec::new())).is_empty(),
        "an empty window raises nothing"
    );
    let below = breakdown(100_000, vec![footprint("bash", 0, 79_000)], Vec::new());
    assert!(
        !titles(&below).iter().any(|t| t.contains("Context is")),
        "79% is below the band: {:?}",
        titles(&below)
    );
    let at = breakdown(100_000, vec![footprint("bash", 0, 80_000)], Vec::new());
    assert!(
        titles(&at)
            .iter()
            .any(|t| t.contains("Context is 80% full")),
        "80% is at the band: {:?}",
        titles(&at)
    );
}

#[test]
fn test_bash_result_warns() {
    let bd = breakdown(100_000, vec![footprint("bash", 1_000, 19_000)], Vec::new());
    let suggestions = suggestions_for(&bd);
    let bash = suggestions
        .iter()
        .find(|s| s.title.contains("Bash results"))
        .expect("a large bash result is reported");
    assert_eq!(bash.severity, SuggestionSeverity::Warning);
    assert!(
        bash.title.contains("20k tokens (20%)"),
        "the title names the tokens and the share: {}",
        bash.title
    );
    assert_eq!(bash.savings_tokens, Some(10_000), "half is reclaimable");
}

#[test]
fn test_webfetch_result_reported() {
    // The tool is declared with capitals, so the advice must match the name as
    // it is recorded rather than a lowercased guess.
    let bd = breakdown(
        100_000,
        vec![footprint("WebFetch", 100, 19_900)],
        Vec::new(),
    );
    assert!(
        titles(&bd).iter().any(|t| t.contains("WebFetch results")),
        "the fetched page is reported under its own name: {:?}",
        titles(&bd)
    );
}

#[test]
fn test_small_tool_silent() {
    // Below either band: a tool that crosses neither the share nor the floor
    // gets no row, and an unlisted tool gets none below a fifth of the window.
    let small = breakdown(100_000, vec![footprint("bash", 100, 9_000)], Vec::new());
    assert!(
        titles(&small).iter().all(|t| !t.contains("Bash")),
        "9k is below the floor: {:?}",
        titles(&small)
    );
    let unlisted = breakdown(
        100_000,
        vec![footprint("multiedit", 1_000, 15_000)],
        Vec::new(),
    );
    assert!(
        titles(&unlisted).iter().all(|t| !t.contains("Multiedit")),
        "an unlisted tool at 16% stays silent: {:?}",
        titles(&unlisted)
    );
    let huge = breakdown(
        100_000,
        vec![footprint("multiedit", 1_000, 21_000)],
        Vec::new(),
    );
    assert!(
        titles(&huge).iter().any(|t| t.contains("Multiedit using")),
        "an unlisted tool past a fifth is named: {:?}",
        titles(&huge)
    );
}

#[test]
fn test_read_bloat_fires() {
    // 12% of the window in read results: under the tool-row band, over the
    // read band and the floor that the read row carries.
    let bd = breakdown(100_000, vec![footprint("read", 500, 12_000)], Vec::new());
    let suggestions = suggestions_for(&bd);
    let read = suggestions
        .iter()
        .filter(|s| s.title.contains("File reads"))
        .count();
    assert_eq!(read, 1, "the read habit is named once: {suggestions:?}");
}

#[test]
fn test_read_bloat_suppressed() {
    // The read row already carries the advice once the tool row fires, so the
    // same measurement must not produce two rows telling the same story.
    let bd = breakdown(100_000, vec![footprint("read", 1_000, 19_000)], Vec::new());
    let suggestions = suggestions_for(&bd);
    assert!(
        suggestions.iter().any(|s| s.title.contains("Read results")),
        "the tool row fires: {suggestions:?}"
    );
    assert!(
        suggestions.iter().all(|s| !s.title.contains("File reads")),
        "and the read habit row stays out: {suggestions:?}"
    );
}

#[test]
fn test_memory_names_largest() {
    let bd = breakdown(
        100_000,
        Vec::new(),
        vec![
            memory_file("MEMORY.md", 4_000),
            memory_file("notes.md", 3_000),
            memory_file("index.md", 1_000),
        ],
    );
    let suggestions = suggestions_for(&bd);
    let memory = suggestions
        .iter()
        .find(|s| s.title.contains("Memory files"))
        .expect("8% of the window in memory files is reported");
    assert!(
        memory.title.contains("8k tokens (8%)"),
        "the total and its share: {}",
        memory.title
    );
    assert!(
        memory
            .detail
            .contains("Largest: MEMORY.md (4k), notes.md (3k), index.md (1k)"),
        "the three largest are named in order: {}",
        memory.detail
    );
}

#[test]
fn test_memory_below_floor() {
    let bd = breakdown(100_000, Vec::new(), vec![memory_file("MEMORY.md", 4_999)]);
    assert!(
        titles(&bd).iter().all(|t| !t.contains("Memory")),
        "under the floor no memory row appears: {:?}",
        titles(&bd)
    );
}

#[test]
fn test_warnings_sort_first() {
    let bd = breakdown(
        100_000,
        vec![
            footprint("bash", 1_000, 19_000),
            footprint("read", 500, 12_000),
        ],
        vec![memory_file("MEMORY.md", 20_000)],
    );
    let severities: Vec<SuggestionSeverity> = suggestions_for(&bd)
        .into_iter()
        .map(|s| s.severity)
        .collect();
    assert!(severities.len() >= 3, "the fixture fires several rows");
    let first_info = severities
        .iter()
        .position(|s| *s == SuggestionSeverity::Info)
        .expect("an informational row is present");
    assert!(
        severities[..first_info]
            .iter()
            .all(|s| *s == SuggestionSeverity::Warning),
        "no warning sorts after an informational row: {severities:?}"
    );
}
