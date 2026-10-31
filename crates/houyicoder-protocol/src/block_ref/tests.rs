//! Where a stored value keeps its block marker, and what the walk returns
//! for each shape the producer writes.

use serde_json::json;

use super::BLOCK_BYTES_KEY;
use super::BLOCK_REF_KEY;
use super::block_addresses;
use super::block_sizes;

/// The marker is stored in logs already written, so its spelling is a
/// persisted format: a rename here would make every older log name nothing
/// and the reclaim pass would free blocks those logs still reference.
#[test]
fn test_marker_key_is_persisted() {
    assert_eq!(BLOCK_REF_KEY, "block_ref");
}

/// A whole output externalized carries its marker at the top level, beside
/// the inline preview the model reads instead of the bytes.
#[test]
fn test_addresses_top_level_marker() {
    let value = json!({ BLOCK_REF_KEY: "a1", "preview": "..." });
    assert_eq!(block_addresses(&value), vec!["a1"]);
}

/// A single large field externalized keeps its envelope and carries the
/// marker as that field's value, so the walk must reach inside.
#[test]
fn test_addresses_nested_field() {
    let value = json!({ "agentId": "c1", "content": { BLOCK_REF_KEY: "b2" } });
    assert_eq!(block_addresses(&value), vec!["b2"]);
}

/// A marker whose value is not a string names no block, so the walk skips it
/// instead of treating the stored value as unreadable.
#[test]
fn test_addresses_skip_non_string() {
    let value = json!({ BLOCK_REF_KEY: 7 });
    assert!(block_addresses(&value).is_empty());
}

/// One value may carry several markers, so the walk returns all of them
/// rather than the first.
#[test]
fn test_addresses_finds_each_marker() {
    let value = json!([{ BLOCK_REF_KEY: "c3" }, { "outer": { BLOCK_REF_KEY: "d4" } }]);
    assert_eq!(block_addresses(&value), vec!["c3", "d4"]);
}

/// A marker carries the stored length beside its address, so a reader that
/// gates on size learns it from the same walk that finds the block.
#[test]
fn test_sizes_reported_beside_address() {
    let value = json!({ BLOCK_REF_KEY: "a1", BLOCK_BYTES_KEY: 7000, "preview": "..." });
    assert_eq!(block_sizes(&value), vec![("a1", Some(7000))]);
}

/// A marker written before the producer recorded a length still names its
/// block; the walk reports the size as absent rather than inventing one.
#[test]
fn test_sizes_absent_when_unnamed() {
    let value = json!({ "content": { BLOCK_REF_KEY: "b2" } });
    assert_eq!(block_sizes(&value), vec![("b2", None)]);
}
