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
