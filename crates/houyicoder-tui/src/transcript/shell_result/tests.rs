//! Tests for this module.

use houyicoder_protocol::block_ref::BLOCK_BYTES_KEY;
use houyicoder_protocol::block_ref::BLOCK_REF_KEY;

use super::*;

fn marker(hash: &str, bytes: u64) -> Value {
    serde_json::json!({
        BLOCK_REF_KEY: hash,
        BLOCK_BYTES_KEY: bytes,
        "preview": "start"
    })
}

/// A whole-output marker names the block and its stored size; the tag takes
/// the eight characters a reader can compare, not the full hash.
#[test]
fn test_tag_names_block() {
    let output = marker("a1b2c3d4e5f60718", 23_040);
    let tag = cas_tag(&output).expect("a marker renders a tag");
    assert_eq!(tag, "CAS a1b2c3d4 (22.5 KiB)");
}

/// A bash result externalizes one field of its envelope rather than the whole
/// output, so the marker sits one level down; the tag must still find it.
#[test]
fn test_tag_finds_nested() {
    let output = serde_json::json!({
        "stdout": marker("feedface00000000", 2048),
        "exit_code": 0
    });
    let tag = cas_tag(&output).expect("a nested marker renders a tag");
    assert_eq!(tag, "CAS feedface (2.0 KiB)");
}

/// An output that was not externalized has nothing to report, so the row
/// carries no tag at all.
#[test]
fn test_tag_absent() {
    let output = serde_json::json!({ "stdout": "ok", "exit_code": 0 });
    assert!(cas_tag(&output).is_none());
}

/// A marker that names its block but not its stored size cannot state a size,
/// so the row stays silent rather than print a number it does not have.
#[test]
fn test_tag_needs_size() {
    let output = serde_json::json!({ BLOCK_REF_KEY: "a1b2c3d4e5f60718" });
    assert!(cas_tag(&output).is_none());
}

/// A capture that published its blocks reports Saved.
#[test]
fn test_storage_saved() {
    let output = serde_json::json!({
        "stdout": "ok",
        CAPTURE_KEY: { RECORD_KEY: marker("abc", 10) }
    });
    assert_eq!(storage_state(&output), Some(ShellStorage::Saved));
}

/// A capture that refused to publish reports SaveFailed even though the
/// command itself succeeded: the bounded text is all that survives.
#[test]
fn test_storage_refused() {
    let output = serde_json::json!({
        "stdout": "ok",
        "exit_code": 0,
        CAPTURE_KEY: { CAPTURE_FAILED_KEY: "not kept" }
    });
    assert_eq!(storage_state(&output), Some(ShellStorage::SaveFailed));
}

/// A capture that stopped the command reports Truncated, because the kept
/// bytes end where the stop happened rather than where the command ended.
#[test]
fn test_storage_truncated() {
    let output = serde_json::json!({
        "stdout": "start",
        CAPTURE_KEY: {
            RECORD_KEY: marker("abc", 10),
            STOP_REASON_KEY: "wall_budget"
        }
    });
    assert_eq!(storage_state(&output), Some(ShellStorage::Truncated));
}

/// A result that names no capture keeps nothing and loses nothing, so the
/// row says nothing about storage.
#[test]
fn test_storage_absent() {
    let output = serde_json::json!({ "stdout": "ok", "exit_code": 0 });
    assert_eq!(storage_state(&output), None);
}
