//! Tests for this module.

use super::*;

#[test]
fn test_reply_round_trips() {
    let reply = CompactReply::new(true, 12, "ckpt_abc", Some(8000), Some(3000));
    let json = serde_json::to_string(&reply).expect("serialize");
    assert!(json.contains("madeProgress"), "camelCase in {json}");
    assert!(json.contains("foldedCount"), "camelCase in {json}");
    assert!(json.contains("manifestId"), "camelCase in {json}");
    assert!(json.contains("preCompactTokens"), "camelCase in {json}");
    assert!(json.contains("postCompactTokens"), "camelCase in {json}");
    let back: CompactReply = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, reply);
}

#[test]
fn test_no_progress_round_trips() {
    // A no-progress reply (all-Verbatim manifest) still carries the
    // manifest id + zero folded + the pre/post estimates so the user
    // sees compaction was a no-op, not a silent failure.
    let reply = CompactReply::new(false, 0, "ckpt_empty", Some(1000), Some(1000));
    let json = serde_json::to_string(&reply).expect("serialize");
    let back: CompactReply = serde_json::from_str(&json).expect("deserialize");
    assert!(!back.made_progress);
    assert_eq!(back.folded_count, 0);
}
