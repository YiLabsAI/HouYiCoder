//! Tests for the capture record: the marker shape the reclaim walk follows,
//! and a round trip through the persisted form.

use super::*;
use crate::block_ref::{BLOCK_REF_KEY, block_addresses};
use serde_json::json;

fn record() -> CaptureRecord {
    CaptureRecord {
        version: CAPTURE_RECORD_VERSION,
        session: "session-1".into(),
        call: "call-1".into(),
        tool: "bash".into(),
        created_ms: Some(1_700_000_000_000),
        exit: Some(0),
        completeness: CaptureCompleteness::Complete,
        stop_reason: None,
        stdout: StreamRecord {
            block: Some(BlockMarker::new("stdout-hash")),
            bytes: 12,
            encoding: StreamEncoding::Raw,
        },
        stderr: StreamRecord {
            block: None,
            bytes: 0,
            encoding: StreamEncoding::Raw,
        },
    }
}

/// The record names each stream block under the key the reclaim walk looks
/// for. That walk never opens stored bytes, so a record kept as a block is
/// reached through the result that names it; this field is written under the
/// walker's key so neither side drifts into a synonym.
#[test]
fn test_marker_walks_back() {
    let stored = serde_json::to_value(record()).expect("the record serializes");
    assert_eq!(
        stored["stdout"]["block"][BLOCK_REF_KEY],
        json!("stdout-hash"),
        "the stream marker is written under the walker's key"
    );
    assert!(
        stored["stderr"]["block"].is_null(),
        "a stream that printed nothing names no block"
    );
    assert_eq!(
        block_addresses(&stored),
        vec!["stdout-hash"],
        "the walk reaches every address the record names"
    );
}

/// A record read back from the store equals the one written, so a reader can
/// resolve a reference through the fields it knows.
#[test]
fn test_record_round_trips() {
    let written = record();
    let bytes = serde_json::to_vec(&written).expect("the record serializes");
    let read = read_capture_record(&bytes).expect("the record reads back");
    assert_eq!(read, written);
    assert_eq!(read.version, CAPTURE_RECORD_VERSION);
}

/// A record from a newer capture format is refused as a version this build
/// does not know. A value added after this build cannot be told from a
/// corrupt one, so the refusal must name the version instead of the shape.
#[test]
fn test_newer_version_is_refused() {
    let mut stored = serde_json::to_value(record()).expect("the record serializes");
    stored["version"] = json!(CAPTURE_RECORD_VERSION + 1);
    stored["stop_reason"] = json!("a reason this build never wrote");
    let bytes = serde_json::to_vec(&stored).expect("the value serializes");
    assert_eq!(
        read_capture_record(&bytes),
        Err(CaptureRecordError::UnsupportedVersion(
            CAPTURE_RECORD_VERSION + 1
        ))
    );
}

/// Bytes that are not a record and a record with no version are separate
/// outcomes, so a caller can tell a broken store from a format it must
/// upgrade to read.
#[test]
fn test_broken_bytes_are_reported() {
    assert!(matches!(
        read_capture_record(b"not json"),
        Err(CaptureRecordError::Unreadable(_))
    ));
    assert_eq!(
        read_capture_record(br#"{"session":"s"}"#),
        Err(CaptureRecordError::Malformed("no version".into()))
    );
}

/// A truncated capture says so, names what stopped it, and carries the kept
/// length of each stream separately: the cut position is known per stream and
/// the total printed is not.
#[test]
fn test_truncated_state_survives() {
    let mut written = record();
    written.exit = None;
    written.completeness = CaptureCompleteness::Truncated;
    written.stop_reason = Some(CaptureStopReason::ByteBudget);
    written.stderr.bytes = 8 * 1024 * 1024;
    written.stderr.block = Some(BlockMarker::new("stderr-hash"));
    let stored = serde_json::to_value(&written).expect("the record serializes");
    assert_eq!(stored["completeness"], json!("truncated"));
    assert_eq!(stored["stop_reason"], json!("byte_budget"));
    assert!(stored["exit"].is_null(), "nothing exited");
    assert_eq!(stored["stderr"]["bytes"], json!(8 * 1024 * 1024));

    written.stop_reason = Some(CaptureStopReason::WallBudget);
    let stored = serde_json::to_value(&written).expect("the record serializes");
    assert_eq!(stored["stop_reason"], json!("wall_budget"));
}

/// A result that published its capture names every block it wrote, the bytes
/// each stream kept, and nothing that stopped it.
#[test]
fn test_reference_reads_published() {
    let output = json!({
        "stdout": "ok",
        CAPTURE_KEY: {
            RECORD_KEY: { BLOCK_REF_KEY: "record-hash" },
            STDOUT_BYTES_KEY: 12,
            STDERR_BYTES_KEY: 0,
            STDOUT_KEY: { BLOCK_REF_KEY: "stdout-hash" },
        }
    });
    let reference = read_capture_reference(&output).expect("the result names a capture");
    assert_eq!(
        reference,
        CaptureReference {
            record: Some("record-hash".into()),
            stdout: Some("stdout-hash".into()),
            stderr: None,
            stdout_bytes: Some(12),
            stderr_bytes: Some(0),
            stop: None,
            failed: false,
        }
    );
}

/// A capture that refused to publish says so and keeps the stop it applied:
/// the command was ended either way, so the bounded result must not read as a
/// complete one.
#[test]
fn test_reference_names_refusal() {
    let output = json!({
        CAPTURE_KEY: {
            CAPTURE_FAILED_KEY: "not kept",
            STOP_REASON_KEY: "wall_budget",
        }
    });
    let reference = read_capture_reference(&output).expect("the result names a capture");
    assert!(reference.failed);
    assert_eq!(reference.stop, Some(CaptureStopReason::WallBudget));
    assert_eq!(reference.record, None);
}

/// An output the model view held whole carries no capture, so there is
/// nothing kept beside it to read.
#[test]
fn test_reference_absent() {
    assert_eq!(read_capture_reference(&json!({ "stdout": "ok" })), None);
}
