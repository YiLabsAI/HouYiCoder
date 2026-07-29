//! Deterministic fact extraction from user input.
//!
//! No model classifier on the hot path. Facts are extracted from explicit
//! save signals the user types deliberately — a structured prefix the user
//! opts into, not a model deciding what to remember. This keeps the agent
//! loop free of per-turn model cost for memory extraction.
//!
//! Supported signal: a line starting with the /save prefix followed by a
//! key, an optional source tag, and the content. Examples:
//!   /save my-key user: Always run tests before committing
//!   /save architecture project: The storage layer owns all disk I/O
//!
//! When no source tag is present the default is User. The content is the
//! remainder of the line after the key (and optional source tag).

use houyicoder_context::{MemoryEntry, MemorySource};

/// Extract save facts from user input. Scans each line for the /save
/// prefix. Returns one MemoryEntry per matching line. Lines without the
/// prefix are ignored — the user input still flows to the model unchanged.
pub fn extract_save_facts(input: &str) -> Vec<MemoryEntry> {
    let mut entries = Vec::new();
    for line in input.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("/save ") else {
            continue;
        };
        if let Some(entry) = parse_save_line(rest) {
            entries.push(entry);
        }
    }
    entries
}

/// Parse the text after the /save prefix into a memory entry. The first
/// token is the key. If the second token looks like a source tag (one of
/// the four known labels followed by a colon) it is consumed as the
/// source; otherwise the source defaults to User and the rest is content.
fn parse_save_line(rest: &str) -> Option<MemoryEntry> {
    let rest = rest.trim();
    if rest.is_empty() {
        return None;
    }
    let mut parts = rest.splitn(2, char::is_whitespace);
    let key = parts.next()?.trim();
    let remainder = parts.next()?.trim();
    if key.is_empty() || remainder.is_empty() {
        return None;
    }
    // Try to peel a source tag: user:, project:, feedback:, reference:.
    let (source, content) = match remainder.split_once(':') {
        Some((tag, body)) if body.chars().next().is_some_and(char::is_whitespace) => {
            let body = body.trim();
            if let Some(src) = MemorySource::from_label(tag.trim()) {
                (src, body)
            } else {
                (MemorySource::User, remainder)
            }
        }
        _ => (MemorySource::User, remainder),
    };
    // Guard against path traversal in the key — the provider's own
    // validator is the real gate, but rejecting early avoids wasted work.
    if key.contains('/') || key.contains('\\') {
        return None;
    }
    Some(MemoryEntry::new(key, content, source))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_single_save() {
        let entries = extract_save_facts("/save my-key user: Always run tests");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, "my-key");
        assert_eq!(entries[0].source, MemorySource::User);
        assert!(entries[0].content.contains("Always run tests"));
    }

    #[test]
    fn test_extract_project_source() {
        let entries = extract_save_facts("/save arch project: Storage owns I/O");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source, MemorySource::Project);
    }

    #[test]
    fn test_extract_default_source() {
        let entries = extract_save_facts("/save pref: No source tag means user");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source, MemorySource::User);
        assert_eq!(entries[0].content, "No source tag means user");
    }

    #[test]
    fn test_no_save_signal_returns() {
        let entries = extract_save_facts("just a normal prompt");
        assert!(entries.is_empty());
    }

    #[test]
    fn test_multiple_saves_in_one() {
        let input = "/save a user: fact one\n/some other line\n/save b project: fact two";
        let entries = extract_save_facts(input);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].key, "a");
        assert_eq!(entries[1].key, "b");
    }

    #[test]
    fn test_rejects_path_in_key() {
        let entries = extract_save_facts("/save ../escape user: bad");
        assert!(entries.is_empty(), "path traversal in key must be rejected");
    }

    #[test]
    fn test_feedback_source_tag() {
        let entries = extract_save_facts("/save review feedback: prefer short functions");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source, MemorySource::Feedback);
    }

    #[test]
    fn test_reference_source_tag() {
        let entries = extract_save_facts("/save book reference: The Rust Book chapter 4");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source, MemorySource::Reference);
    }
}
