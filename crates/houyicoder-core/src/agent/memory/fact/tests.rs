//! Tests for this module.

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
