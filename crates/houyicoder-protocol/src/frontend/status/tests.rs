//! Tests for the status snapshot's shape on the way in.

use super::*;

/// A snapshot payload that carries no key for the per-model provenance field
/// reads as all-unreported, so the Usage tab claims no share over a count the
/// endpoints behind that model may never have reported.
#[test]
fn test_legacy_row_unreported() {
    let legacy = r#"{"model":"glm-5.2","inputTokens":1000,"outputTokens":0,
        "cacheReadTokens":0,"cacheWriteTokens":0,"reasoningTokens":0}"#;
    let row: ModelUsageView = serde_json::from_str(legacy).expect("a legacy per-model row");
    assert_eq!(
        row.provenance,
        UsageProvenance::default(),
        "a row with no provenance key reads as all-unreported"
    );
    assert!(!row.provenance.cache_account_complete());
}

/// A snapshot from a server that predates the refusal count still decodes,
/// and the absent count reads as no refusals rather than failing the whole
/// payload. A count the server does send survives the round trip.
#[test]
fn test_refusal_count_backward_compatible() {
    // Drop the key from a current payload: what remains is what a server
    // that predates the count sends.
    let mut payload = serde_json::to_value(StatusSnapshot {
        tool_calls: 3,
        tool_success: 1,
        tool_errors: 1,
        tool_refused: 1,
        ..Default::default()
    })
    .expect("encode");
    let object = payload
        .as_object_mut()
        .expect("a status payload is an object");
    object.remove("toolRefused");
    let legacy: StatusSnapshot = serde_json::from_value(payload).expect("a legacy status payload");
    assert_eq!(legacy.tool_refused, 0, "an absent count means none");
    assert_eq!(legacy.tool_calls, 3, "the rest of the payload decodes");

    let encoded = serde_json::to_string(&StatusSnapshot {
        tool_refused: 1,
        ..Default::default()
    })
    .expect("encode");
    let round_trip: StatusSnapshot = serde_json::from_str(&encoded).expect("decode");
    assert_eq!(round_trip.tool_refused, 1);
}
